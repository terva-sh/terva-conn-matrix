//! Live compliance suite against the throwaway Synapse (`just e2e`).
//!
//! Exercises everything the wiremock tests cannot: the real binary's
//! headless `setup`, session restore, the connect-time backlog discard, the
//! live sync loop, invite auto-join with the `is_direct` DM signal, and the
//! full DM text round trip — against an actual homeserver, with a real
//! second client playing the human.
//!
//! Gated twice so ordinary `cargo test` stays hermetic: the tests are
//! `#[ignore]`d, and they hard-require `TERVA_CONN_MATRIX_E2E_HS` (set by
//! `just e2e` to the compose server, e.g. `http://127.0.0.1:18008`).

// matrix-sdk's deeply nested futures overflow the default trait-solver
// recursion limit when proving Send/Sync.
#![recursion_limit = "256"]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use matrix_sdk::attachment::AttachmentConfig;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::{IncludeRelations, RelationsOptions};
use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
use matrix_sdk::ruma::api::client::membership::joined_members;
use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoomRequest;
use matrix_sdk::ruma::api::client::room::get_room_event;
use matrix_sdk::ruma::api::client::uiaa::{AuthData, Dummy};
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::{Annotation, RelationType, Reply, Thread};
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, ReplacementMetadata,
    RoomMessageEventContent, TextMessageEventContent,
};
use matrix_sdk::ruma::events::typing::SyncTypingEvent;
use matrix_sdk::ruma::events::{Mentions, TimelineEventType};
use matrix_sdk::ruma::{EventId, OwnedUserId, UserId};
use matrix_sdk::{Client, Room};
use tokio::runtime::Runtime;

fn homeserver() -> String {
    std::env::var("TERVA_CONN_MATRIX_E2E_HS")
        .expect("TERVA_CONN_MATRIX_E2E_HS not set — run this suite via `just e2e`")
}

fn unique(prefix: &str) -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    format!("{prefix}-{}-{ms}", std::process::id())
}

/// Register a throwaway account (open registration, UIAA dummy stage).
async fn register_account(hs: &str, localpart: &str, password: &str) {
    let client = Client::builder()
        .homeserver_url(hs)
        .build()
        .await
        .expect("build");
    let mut request = RegisterRequest::new();
    request.username = Some(localpart.to_owned());
    request.password = Some(password.to_owned());
    request.auth = Some(AuthData::Dummy(Dummy::new()));
    client
        .matrix_auth()
        .register(request)
        .await
        .expect("register account (is the throwaway synapse up?)");
}

/// A message the human client observed, via its own sync.
#[derive(Debug, Clone)]
struct SeenMessage {
    event_id: String,
    body: String,
    html: Option<String>,
    /// The msgtype ("m.text", "m.image", …) — attachments record their
    /// caption/filename as `body`.
    kind: String,
}

/// The human side of the conversation: a real matrix-sdk client with a
/// background sync, collecting messages and typing notifications.
struct Human {
    client: Client,
    user_id: OwnedUserId,
    seen: Arc<Mutex<Vec<SeenMessage>>>,
    typing: Arc<Mutex<Vec<String>>>,
}

impl Human {
    async fn register(hs: &str, localpart: &str, password: &str) -> Human {
        let client = Client::builder()
            .homeserver_url(hs)
            .build()
            .await
            .expect("build human client");
        let mut request = RegisterRequest::new();
        request.username = Some(localpart.to_owned());
        request.password = Some(password.to_owned());
        request.auth = Some(AuthData::Dummy(Dummy::new()));
        client
            .matrix_auth()
            .register(request)
            .await
            .expect("register human (is the throwaway synapse up?)");
        let user_id = client.user_id().expect("registered").to_owned();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let typing = Arc::new(Mutex::new(Vec::new()));
        client.add_event_handler({
            let seen = seen.clone();
            move |ev: OriginalSyncRoomMessageEvent| {
                let seen = seen.clone();
                async move {
                    let html = match &ev.content.msgtype {
                        MessageType::Text(text) => text.formatted.as_ref().map(|f| f.body.clone()),
                        _ => None,
                    };
                    seen.lock().unwrap().push(SeenMessage {
                        event_id: ev.event_id.to_string(),
                        body: ev.content.body().to_string(),
                        html,
                        kind: ev.content.msgtype().to_string(),
                    });
                }
            }
        });
        client.add_event_handler({
            let typing = typing.clone();
            move |ev: SyncTypingEvent| {
                let typing = typing.clone();
                async move {
                    // m.typing carries the whole current set each time, so
                    // replacing (not appending) is what makes a STOP
                    // observable: the set the bot vanishes from.
                    let mut typing = typing.lock().unwrap();
                    *typing = ev.content.user_ids.iter().map(|u| u.to_string()).collect();
                }
            }
        });
        tokio::spawn({
            let client = client.clone();
            async move {
                let _ = client.sync(SyncSettings::new()).await;
            }
        });
        Human {
            client,
            user_id,
            seen,
            typing,
        }
    }

    async fn open_dm(&self, bot: &UserId) -> Room {
        let mut request = CreateRoomRequest::new();
        request.is_direct = true;
        request.invite = vec![bot.to_owned()];
        self.client
            .create_room(request)
            .await
            .expect("create DM room")
    }

    /// A named multi-user room with the bot invited — a `group` chat.
    async fn open_group(&self, name: &str, bot: &UserId) -> Room {
        let mut request = CreateRoomRequest::new();
        request.name = Some(name.to_owned());
        request.invite = vec![bot.to_owned()];
        self.client
            .create_room(request)
            .await
            .expect("create group room")
    }

    /// A message with an intentional `m.mentions` of `mentioned` — the way
    /// Matrix v1.7 clients mention someone.
    async fn send_mention(&self, room: &Room, body: &str, mentioned: &UserId) -> String {
        let mut content = RoomMessageEventContent::text_plain(body);
        content.mentions = Some(Mentions::with_user_ids([mentioned.to_owned()]));
        room.send(content)
            .await
            .expect("human mention send")
            .response
            .event_id
            .to_string()
    }

    async fn send_text(&self, room: &Room, body: &str) -> String {
        room.send(RoomMessageEventContent::text_plain(body))
            .await
            .expect("human send")
            .response
            .event_id
            .to_string()
    }

    /// A rich reply the way real clients send them: fallback quote included.
    async fn send_reply(&self, room: &Room, target: &str, body: &str) -> String {
        let fallback = format!("> <{}> (quoted)\n\n{body}", self.user_id);
        let mut content = RoomMessageEventContent::text_plain(fallback);
        content.relates_to = Some(Relation::Reply(Reply::with_event_id(
            EventId::parse(target).expect("target event id"),
        )));
        room.send(content)
            .await
            .expect("human reply")
            .response
            .event_id
            .to_string()
    }

    /// Edit an earlier message of ours the way clients do (`m.replace`).
    async fn send_edit(&self, room: &Room, target: &str, new_body: &str) -> String {
        let content = RoomMessageEventContent::text_plain(new_body).make_replacement(
            ReplacementMetadata::new(
                EventId::parse(target).expect("target event id").to_owned(),
                None,
            ),
        );
        room.send(content)
            .await
            .expect("human edit")
            .response
            .event_id
            .to_string()
    }

    /// React to a message; returns the reaction event id (redact it to
    /// un-react).
    async fn send_reaction(&self, room: &Room, target: &str, key: &str) -> String {
        let content = ReactionEventContent::new(Annotation::new(
            EventId::parse(target).expect("target event id").to_owned(),
            key.to_owned(),
        ));
        room.send(content)
            .await
            .expect("human reaction")
            .response
            .event_id
            .to_string()
    }

    async fn redact(&self, room: &Room, target: &str) {
        room.redact(
            &EventId::parse(target).expect("target event id"),
            None,
            None,
        )
        .await
        .expect("human redact");
    }

    /// A message inside a thread: the fallback shape when `reply_to` is
    /// None, a real in-thread reply otherwise.
    async fn send_thread_text(
        &self,
        room: &Room,
        root: &str,
        body: &str,
        reply_to: Option<&str>,
    ) -> String {
        let root = EventId::parse(root).expect("thread root").to_owned();
        let mut content = RoomMessageEventContent::text_plain(body);
        content.relates_to = Some(Relation::Thread(match reply_to {
            Some(target) => Thread::reply(
                root,
                EventId::parse(target).expect("reply target").to_owned(),
            ),
            None => Thread::plain(root.clone(), root),
        }));
        room.send(content)
            .await
            .expect("human thread send")
            .response
            .event_id
            .to_string()
    }

    /// Upload + send an image with a caption, the Matrix v1.10 way.
    async fn send_image_attachment(
        &self,
        room: &Room,
        bytes: &[u8],
        filename: &str,
        caption: &str,
    ) -> String {
        let config = AttachmentConfig::new().caption(Some(TextMessageEventContent::plain(caption)));
        room.send_attachment(filename, &mime::IMAGE_PNG, bytes.to_vec(), config)
            .await
            .expect("human attachment send")
            .event_id
            .to_string()
    }

    /// How many un-redacted reactions `sender` currently has on `target`,
    /// straight from the server's `/relations` — the widget's visible state.
    async fn count_reactions_by(&self, room: &Room, target: &str, sender: &str) -> usize {
        let opts = RelationsOptions {
            include_relations: IncludeRelations::RelationsOfTypeAndEventType(
                RelationType::Annotation,
                TimelineEventType::Reaction,
            ),
            ..Default::default()
        };
        let relations = room
            .relations(
                EventId::parse(target).expect("target event id").to_owned(),
                opts,
            )
            .await
            .expect("relations fetch");
        relations
            .chunk
            .iter()
            .filter_map(|ev| ev.raw().deserialize_as::<serde_json::Value>().ok())
            .filter(|v| v["sender"] == sender && !v["content"]["m.relates_to"].is_null())
            .count()
    }

    /// The full raw event JSON as the server stores it.
    async fn raw_event(&self, room: &Room, event_id: &str) -> serde_json::Value {
        let request = get_room_event::v3::Request::new(
            room.room_id().to_owned(),
            EventId::parse(event_id).expect("event id").to_owned(),
        );
        let response = self.client.send(request).await.expect("raw event fetch");
        response.event.deserialize_as().expect("event is JSON")
    }

    /// Wait until the human has seen some message whose body satisfies
    /// `pred` — for events whose id we don't learn from a result frame.
    fn wait_for_body(&self, what: &str, timeout: Duration, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.seen.lock().unwrap().iter().any(|m| pred(&m.body)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "human never saw {what}; saw: {:?}",
            self.seen.lock().unwrap()
        );
    }

    /// The event exactly as the server stores it — this path never touches
    /// the Olm machine, so `"type"` proves cipher vs plain text on the wire.
    async fn raw_event_type(&self, room: &Room, event_id: &str) -> String {
        let request = get_room_event::v3::Request::new(
            room.room_id().to_owned(),
            EventId::parse(event_id).expect("event id").to_owned(),
        );
        let response = self.client.send(request).await.expect("raw event fetch");
        response
            .event
            .get_field::<String>("type")
            .expect("parse type")
            .expect("type present")
    }

    fn wait_for_message(&self, event_id: &str, timeout: Duration) -> SeenMessage {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(m) = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .find(|m| m.event_id == event_id)
            {
                return m.clone();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "human never saw event {event_id}; saw: {:?}",
            self.seen.lock().unwrap()
        );
    }

    fn wait_for_typing(&self, user: &UserId, timeout: Duration) -> bool {
        self.wait_typing_state(user, true, timeout)
    }

    fn wait_for_typing_cleared(&self, user: &UserId, timeout: Duration) -> bool {
        self.wait_typing_state(user, false, timeout)
    }

    fn wait_typing_state(&self, user: &UserId, want: bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let typing = self
                .typing
                .lock()
                .unwrap()
                .iter()
                .any(|u| u == user.as_str());
            if typing == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }
}

/// The host side of the wire: the real binary under `run`, frames with
/// deadlines (a reader thread feeds a channel so nothing blocks forever).
struct Wire {
    child: Child,
    stdin: ChildStdin,
    frames: mpsc::Receiver<serde_json::Value>,
}

impl Wire {
    fn spawn(home: &Path, log_name: &str) -> Wire {
        let log = fs::File::create(home.join(log_name)).expect("stderr log");
        let mut child = Command::new(env!("CARGO_BIN_EXE_terva-conn-matrix"))
            .arg("run")
            .env("TERVA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn connector");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if tx.send(value).is_err() {
                    return;
                }
            }
        });
        Wire {
            child,
            stdin,
            frames,
        }
    }

    fn write(&mut self, frame: serde_json::Value) {
        let mut line = frame.to_string().into_bytes();
        line.push(b'\n');
        self.stdin.write_all(&line).expect("write frame");
    }

    /// Next frame matching `pred` within `timeout`; every non-matching frame
    /// is remembered for the panic message.
    fn expect_frame(
        &self,
        what: &str,
        timeout: Duration,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + timeout;
        let mut skipped = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(left) {
                Ok(frame) if pred(&frame) => return frame,
                Ok(frame) => skipped.push(frame),
                Err(_) => panic!("no {what} frame within {timeout:?}; saw: {skipped:?}"),
            }
        }
    }

    /// Drain frames for `window`, returning everything seen (for negative
    /// assertions like "no backlog delivered").
    fn drain_for(&self, window: Duration) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + window;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return seen;
            }
            if let Ok(frame) = self.frames.recv_timeout(left) {
                seen.push(frame);
            } else {
                return seen;
            }
        }
    }

    /// Returns the `hello` frame so scenarios can assert on declarations.
    fn handshake_and_connect(&mut self, home: &Path, bot_user: &str) -> serde_json::Value {
        let hello = self.expect_frame("hello", Duration::from_secs(5), |f| f["type"] == "hello");
        assert_eq!(hello["name"], "matrix");
        self.write(serde_json::json!({
            "type": "hello_ack",
            "protocol": 2,
            "terva_version": "e2e",
            "data_dir": home.join("connectors/matrix/data").to_str().unwrap(),
            "capabilities": {"features": [
                "message_ids", "chat_kinds", "asks", "entities", "chat_membership",
                "edits_in", "deletes_in", "reactions_in", "attachment_kinds",
            ]},
        }));
        self.write(serde_json::json!({"type": "connect"}));
        let connected = self.expect_frame("connected", Duration::from_secs(30), |f| {
            f["type"] == "connected" || f["type"] == "connect_error"
        });
        assert_eq!(
            connected["type"], "connected",
            "connect failed: {connected:?}"
        );
        assert_eq!(connected["id"], bot_user);
        hello
    }

    /// The contract requires a prompt exit on `shutdown` (the host escalates
    /// to SIGTERM after ~2 s) — a slow exit is a compliance failure, not
    /// something to wait out.
    fn shutdown(mut self) {
        self.write(serde_json::json!({"type": "shutdown"}));
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => {
                    assert!(status.success(), "connector exited {status:?}");
                    return;
                }
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    panic!("connector did not exit within 10 s of shutdown — the prompt-exit contract is violated");
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

/// Provision the connector against the throwaway homeserver by driving the
/// real `setup` verb headlessly (three piped answers; the password prompt
/// falls back to a plain stdin line off-tty).
fn provision(home: &Path, hs: &str, localpart: &str, password: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terva-conn-matrix"))
        .arg("setup")
        .env("TERVA_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn setup");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{hs}\n{localpart}\n{password}\n").as_bytes())
        .expect("pipe setup answers");
    let out = child.wait_with_output().expect("setup output");
    assert!(
        out.status.success(),
        "setup failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let configured = Command::new(env!("CARGO_BIN_EXE_terva-conn-matrix"))
        .arg("configured")
        .env("TERVA_HOME", home)
        .status()
        .expect("run configured");
    assert!(configured.success(), "configured must exit 0 after setup");
}

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn dm_round_trip_compliance() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    // -- Provision: bot registered on the throwaway server, then the real
    // `setup` verb logs it in and persists the session.
    let bot_local = unique("bot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");

    // -- The human: a second real client with its own sync.
    let human = runtime.block_on(Human::register(&hs, &unique("drew"), "human-pw"));

    // -- Session 1: connect, get invited to a DM, receive, send, reply, type.
    let bot_id = UserId::parse(&bot_user).expect("bot id");
    let mut wire = Wire::spawn(&home, "run-1.log");
    wire.handshake_and_connect(&home, &bot_user);

    let dm = runtime.block_on(human.open_dm(&bot_id));
    let hello_id = runtime.block_on(human.send_text(&dm, "hello agent"));

    // Auto-join + inbound translation + the is_direct DM signal.
    let msg = wire.expect_frame("message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["text"] == "hello agent"
    });
    assert_eq!(msg["id"], hello_id.as_str());
    assert_eq!(msg["chat_id"], dm.room_id().as_str());
    assert_eq!(msg["chat_kind"], "dm", "invite is_direct must map to dm");
    assert_eq!(msg["user_id"], human.user_id.as_str());

    // Outbound send: markdown rendered, result carries the event id, and the
    // human's client actually receives it.
    wire.write(serde_json::json!({
        "type": "send", "id": "e2e-1",
        "chat_id": dm.room_id().as_str(),
        "reply_to": hello_id.as_str(),
        "text": "**hello** human",
    }));
    let result = wire.expect_frame("result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "e2e-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let bot_event = result["message_id"]
        .as_str()
        .expect("message_id")
        .to_string();
    let delivered = human.wait_for_message(&bot_event, Duration::from_secs(30));
    assert_eq!(delivered.body, "**hello** human");
    let html = delivered.html.expect("markdown must render an HTML body");
    assert!(html.contains("<strong>hello</strong>"), "got: {html}");

    // Rich reply back: reply_to mapped, fallback quote stripped.
    let reply_id = runtime.block_on(human.send_reply(&dm, &bot_event, "got it"));
    let reply = wire.expect_frame("reply message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == reply_id.as_str()
    });
    assert_eq!(reply["reply_to"], bot_event.as_str());
    assert_eq!(reply["text"], "got it", "fallback quote must be stripped");

    // Typing indicator reaches the human's sync.
    wire.write(serde_json::json!({"type": "typing", "chat_id": dm.room_id().as_str()}));
    assert!(
        human.wait_for_typing(&bot_id, Duration::from_secs(15)),
        "human never saw the bot typing"
    );
    // ...and the host's typing_stop clears it at once, instead of letting
    // the 30 s server timeout run out beside the delivered reply.
    wire.write(serde_json::json!({
        "type": "typing", "chat_id": dm.room_id().as_str(), "active": false
    }));
    assert!(
        human.wait_for_typing_cleared(&bot_id, Duration::from_secs(15)),
        "typing indicator still lit after the stop"
    );

    wire.shutdown();

    // -- Session 2: downtime recovery. A message sent while the connector
    // is down must be DELIVERED after the restart (the dogfood found that
    // silent loss reads as a broken bridge, not discipline — only the
    // FIRST-ever connect discards history), and live traffic must flow.
    let missed_id = runtime.block_on(human.send_text(&dm, "sent while you were away"));
    let mut wire = Wire::spawn(&home, "run-2.log");
    wire.handshake_and_connect(&home, &bot_user);

    let missed = wire.expect_frame("recovered message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == missed_id.as_str()
    });
    assert_eq!(
        missed["text"], "sent while you were away",
        "downtime must be recovered on reconnect"
    );

    let fresh_id = runtime.block_on(human.send_text(&dm, "fresh after restart"));
    let fresh = wire.expect_frame("post-restart message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == fresh_id.as_str()
    });
    assert_eq!(fresh["text"], "fresh after restart");
    assert_eq!(
        fresh["chat_kind"], "dm",
        "m.direct must survive the restart"
    );

    // -- Cold owner addressing: until the owner's first DM of a host run,
    // terva addresses owner-directed frames (admission asks, approvals,
    // the idle nudge) by the paired USER id (connproto-proposals §12).
    // The connector resolves a user-id chat id to the m.direct DM room.
    let human_id = human.client.user_id().expect("human id").to_string();
    wire.write(serde_json::json!({
        "type": "send", "id": "cold-1",
        "chat_id": human_id.as_str(),
        "text": "addressed by user id",
    }));
    let result = wire.expect_frame("cold-address result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "cold-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    human.wait_for_body("cold-addressed send", Duration::from_secs(30), |body| {
        body == "addressed by user id"
    });

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn encrypted_dm_round_trip() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e-enc"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    // Headless provisioning defaults to [1] create a new recovery key, so
    // this exercises the cross-signing bootstrap + backup path too.
    let bot_local = unique("encbot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");
    let bot_id = UserId::parse(&bot_user).expect("bot id");

    let human = runtime.block_on(Human::register(&hs, &unique("encdrew"), "human-pw"));

    let mut wire = Wire::spawn(&home, "run-1.log");
    wire.handshake_and_connect(&home, &bot_user);

    // Encrypted DM: create, invite, turn on encryption, then WAIT for the
    // bot to join before sending — the sender shares the room key with the
    // devices it can see at send time.
    let dm = runtime.block_on(async {
        let dm = human.open_dm(&bot_id).await;
        dm.enable_encryption().await.expect("enable encryption");
        dm
    });
    // Membership via a raw /joined_members request each time — the SDK's
    // member caches and sync summaries both go stale here. Encryption via
    // the human's CACHED state on purpose: its own send path consults that
    // cache, so it must have flipped before we let the human send.
    let ready = runtime.block_on(async {
        let mut joined = 0;
        let mut encrypted = false;
        for _ in 0..100 {
            joined = human
                .client
                .send(joined_members::v3::Request::new(dm.room_id().to_owned()))
                .await
                .map(|response| response.joined.len())
                .unwrap_or(0);
            encrypted = dm
                .latest_encryption_state()
                .await
                .map(|state| state.is_encrypted())
                .unwrap_or(false);
            if encrypted && joined >= 2 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        eprintln!("readiness stuck: joined={joined} encrypted={encrypted}");
        false
    });
    assert!(ready, "room never became encrypted with the bot joined");

    // Inbound: the bot decrypts transparently…
    let hello_id = runtime.block_on(human.send_text(&dm, "secret hello"));
    let msg = wire.expect_frame("decrypted message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["text"] == "secret hello"
    });
    assert_eq!(msg["id"], hello_id.as_str());
    assert_eq!(msg["chat_kind"], "dm");
    // …while the server only ever saw ciphertext.
    assert_eq!(
        runtime.block_on(human.raw_event_type(&dm, &hello_id)),
        "m.room.encrypted",
        "the human's message must be encrypted on the wire"
    );

    // Outbound: the bot's reply is encrypted on the wire and readable by
    // the human.
    wire.write(serde_json::json!({
        "type": "send", "id": "enc-1",
        "chat_id": dm.room_id().as_str(),
        "text": "**classified** reply",
    }));
    let result = wire.expect_frame("result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "enc-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let bot_event = result["message_id"]
        .as_str()
        .expect("message_id")
        .to_string();
    let delivered = human.wait_for_message(&bot_event, Duration::from_secs(30));
    assert_eq!(delivered.body, "**classified** reply");
    assert_eq!(
        runtime.block_on(human.raw_event_type(&dm, &bot_event)),
        "m.room.encrypted",
        "the bot's reply must be encrypted on the wire"
    );

    wire.shutdown();

    // Crypto-store persistence + downtime recovery: a fresh process must
    // deliver what it missed — decrypted, the room key having arrived via
    // the queued to-device traffic — and keep decrypting live messages.
    let missed = runtime.block_on(human.send_text(&dm, "encrypted while away"));
    let mut wire = Wire::spawn(&home, "run-2.log");
    wire.handshake_and_connect(&home, &bot_user);
    let recovered = wire.expect_frame("recovered encrypted", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == missed.as_str()
    });
    assert_eq!(
        recovered["text"], "encrypted while away",
        "missed ciphertext must decrypt and deliver on reconnect"
    );
    let fresh_id = runtime.block_on(human.send_text(&dm, "fresh encrypted"));
    let fresh = wire.expect_frame(
        "post-restart decrypted message",
        Duration::from_secs(30),
        |f| f["type"] == "message" && f["id"] == fresh_id.as_str(),
    );
    assert_eq!(fresh["text"], "fresh encrypted");

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn group_admission_and_mention_signals() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e-grp"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    let bot_local = unique("groupbot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");
    let bot_id = UserId::parse(&bot_user).expect("bot id");

    let human = runtime.block_on(Human::register(&hs, &unique("gdrew"), "human-pw"));

    let mut wire = Wire::spawn(&home, "run-1.log");
    wire.handshake_and_connect(&home, &bot_user);

    // -- Admission: invite to a named group → auto-join → chat_membership
    // `added`, attributed to the inviter. This is the host's hook to run its
    // approval flow the moment the bot lands in a group.
    let group = runtime.block_on(human.open_group("ops", &bot_id));
    let added = wire.expect_frame("chat_membership added", Duration::from_secs(30), |f| {
        f["type"] == "chat_membership" && f["change"] == "added"
    });
    assert_eq!(added["chat"]["id"], group.room_id().as_str());
    assert_eq!(added["chat"]["kind"], "group");
    assert_eq!(added["chat"]["title"], "ops");
    assert_eq!(added["by_user_id"], human.user_id.as_str());

    // -- An unaddressed group message carries no entities: the host's
    // mention gate must see silence, not a false positive.
    let plain_id = runtime.block_on(human.send_text(&group, "just chatting"));
    let plain = wire.expect_frame("plain group message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == plain_id.as_str()
    });
    assert_eq!(plain["chat_kind"], "group");
    assert_eq!(plain["chat_title"], "ops");
    assert_eq!(
        plain["entities"],
        serde_json::Value::Null,
        "no mention signals → no entities: {plain:?}"
    );

    // -- An m.mentions message becomes a located bot_mention.
    let body = format!("hey {bot_user} deploy it");
    let mention_id = runtime.block_on(human.send_mention(&group, &body, &bot_id));
    let mention = wire.expect_frame("mention message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == mention_id.as_str()
    });
    let ents = mention["entities"].as_array().expect("entities present");
    assert_eq!(ents.len(), 1, "{ents:?}");
    assert_eq!(ents[0]["kind"], "bot_mention");
    assert_eq!(ents[0]["offset"], 4);
    assert_eq!(ents[0]["length"], bot_user.chars().count() as i64);

    // -- A reply to the bot's own message reads as a (span-less) mention.
    wire.write(serde_json::json!({
        "type": "send", "id": "grp-1",
        "chat_id": group.room_id().as_str(),
        "text": "deployed",
    }));
    let result = wire.expect_frame("result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "grp-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let bot_event = result["message_id"]
        .as_str()
        .expect("message_id")
        .to_string();
    let reply_id = runtime.block_on(human.send_reply(&group, &bot_event, "thanks bot"));
    let reply = wire.expect_frame("reply message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == reply_id.as_str()
    });
    assert_eq!(reply["reply_to"], bot_event.as_str());
    let ents = reply["entities"].as_array().expect("reply entities");
    assert!(
        ents.iter()
            .any(|e| e["kind"] == "bot_mention" && e["offset"] == 0 && e["length"] == 0),
        "reply-to-bot must read as a present-but-unlocatable mention: {ents:?}"
    );

    // -- Removal: a kick becomes chat_membership `removed`, attributed to
    // the kicker (the host auto-revokes the group's admission on this).
    runtime
        .block_on(group.kick_user(&bot_id, Some("dogfood over")))
        .expect("kick the bot");
    let removed = wire.expect_frame("chat_membership removed", Duration::from_secs(30), |f| {
        f["type"] == "chat_membership" && f["change"] == "removed"
    });
    assert_eq!(removed["chat"]["id"], group.room_id().as_str());
    assert_eq!(removed["by_user_id"], human.user_id.as_str());

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}

/// A minimal valid 1×1 PNG — servers store it without complaint.
static TINY_PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R',
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE,
    0x42, 0x60, 0x82,
];

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn dm_message_events_and_attachments() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e-ev"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    let bot_local = unique("evbot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");
    let bot_id = UserId::parse(&bot_user).expect("bot id");

    let human = runtime.block_on(Human::register(&hs, &unique("evdrew"), "human-pw"));

    let mut wire = Wire::spawn(&home, "run-1.log");
    wire.handshake_and_connect(&home, &bot_user);

    let dm = runtime.block_on(human.open_dm(&bot_id));
    let hello_id = runtime.block_on(human.send_text(&dm, "hello events"));
    let msg = wire.expect_frame("message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == hello_id.as_str()
    });
    assert_eq!(msg["chat_kind"], "dm");

    wire.write(serde_json::json!({
        "type": "send", "id": "ev-1",
        "chat_id": dm.room_id().as_str(),
        "text": "roger",
    }));
    let result = wire.expect_frame("result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ev-1"
    });
    let bot_msg = result["message_id"].as_str().expect("id").to_string();

    // -- Edits in: delivered under the ORIGINAL id with the new text.
    runtime.block_on(human.send_edit(&dm, &hello_id, "hello events v2"));
    let edited = wire.expect_frame("message_edited", Duration::from_secs(30), |f| {
        f["type"] == "message_edited"
    });
    assert_eq!(edited["id"], hello_id.as_str());
    assert_eq!(edited["text"], "hello events v2");
    assert_eq!(edited["chat_id"], dm.room_id().as_str());

    // -- Reactions in, both directions of the toggle.
    let reaction_event = runtime.block_on(human.send_reaction(&dm, &bot_msg, "👍"));
    let reaction = wire.expect_frame("reaction", Duration::from_secs(30), |f| {
        f["type"] == "reaction" && f["removed"] != true
    });
    assert_eq!(reaction["message_id"], bot_msg.as_str());
    assert_eq!(reaction["key"], "👍");
    assert_eq!(reaction["user_id"], human.user_id.as_str());

    runtime.block_on(human.redact(&dm, &reaction_event));
    let removed = wire.expect_frame("reaction removed", Duration::from_secs(30), |f| {
        f["type"] == "reaction" && f["removed"] == true
    });
    assert_eq!(removed["key"], "👍");
    assert_eq!(removed["message_id"], bot_msg.as_str());
    assert_eq!(
        removed["user_id"],
        human.user_id.as_str(),
        "removal is attributed to the reactor"
    );

    // -- Deletes in.
    let doomed = runtime.block_on(human.send_text(&dm, "delete me"));
    wire.expect_frame("doomed message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == doomed.as_str()
    });
    runtime.block_on(human.redact(&dm, &doomed));
    let deleted = wire.expect_frame("message_deleted", Duration::from_secs(30), |f| {
        f["type"] == "message_deleted"
    });
    assert_eq!(deleted["id"], doomed.as_str());

    // -- React out (the removal happens next session, exercising the
    // /relations rebuild instead of the in-memory cache).
    wire.write(serde_json::json!({
        "type": "react", "id": "ev-2",
        "chat_id": dm.room_id().as_str(),
        "message_id": hello_id.as_str(),
        "key": "👀",
    }));
    let result = wire.expect_frame("react result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ev-2"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");

    // -- Edit out: markdown replacement lands on the human's client.
    wire.write(serde_json::json!({
        "type": "edit", "id": "ev-3",
        "chat_id": dm.room_id().as_str(),
        "message_id": bot_msg.as_str(),
        "text": "**fixed** roger",
    }));
    let result = wire.expect_frame("edit result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ev-3"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    human.wait_for_body("the bot's edit", Duration::from_secs(30), |body| {
        body.starts_with("* ") && body.contains("fixed")
    });

    // -- Delete out: the server's copy of the bot message empties.
    wire.write(serde_json::json!({
        "type": "delete", "id": "ev-4",
        "chat_id": dm.room_id().as_str(),
        "message_id": bot_msg.as_str(),
    }));
    let result = wire.expect_frame("delete result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ev-4"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let raw = runtime.block_on(human.raw_event(&dm, &bot_msg));
    assert_eq!(
        raw["content"]["body"],
        serde_json::Value::Null,
        "redacted content must be empty: {raw}"
    );

    // -- Attachments out: a document with a caption…
    let notes_path = home.join("notes.txt");
    fs::write(&notes_path, b"release notes").expect("write notes");
    wire.write(serde_json::json!({
        "type": "send_file", "id": "ev-5",
        "chat_id": dm.room_id().as_str(),
        "path": notes_path.to_str().unwrap(),
        "caption": "the notes",
    }));
    let result = wire.expect_frame("send_file result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ev-5"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let file_event = result["message_id"].as_str().expect("id").to_string();
    let seen = human.wait_for_message(&file_event, Duration::from_secs(30));
    assert_eq!(seen.kind, "m.file", "text/plain routes to m.file");
    assert_eq!(seen.body, "the notes", "caption becomes the body");

    // …and an image, which must render as m.image.
    let shot_path = home.join("shot.png");
    fs::write(&shot_path, TINY_PNG).expect("write png");
    wire.write(serde_json::json!({
        "type": "send_image", "id": "ev-6",
        "chat_id": dm.room_id().as_str(),
        "path": shot_path.to_str().unwrap(),
    }));
    let result = wire.expect_frame("send_image result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ev-6"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let image_event = result["message_id"].as_str().expect("id").to_string();
    let seen = human.wait_for_message(&image_event, Duration::from_secs(30));
    assert_eq!(seen.kind, "m.image", "png routes to m.image");

    // -- Attachments in: the human's image lands in data_dir, typed and
    // captioned, bytes intact.
    let sent_img = runtime.block_on(human.send_image_attachment(&dm, TINY_PNG, "dot.png", "a dot"));
    let img_msg = wire.expect_frame("attachment message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == sent_img.as_str()
    });
    let atts = img_msg["attachments"].as_array().expect("attachments");
    assert_eq!(atts.len(), 1, "{atts:?}");
    let att = &atts[0];
    assert_eq!(att["kind"], "image");
    assert_eq!(att["name"], "dot.png");
    assert_eq!(att["caption"], "a dot");
    assert_eq!(att["mime_type"], "image/png");
    let path = att["path"].as_str().expect("path");
    let data_dir = home.join("connectors/matrix/data");
    assert!(
        Path::new(path).starts_with(&data_dir),
        "attachment must land in data_dir: {path}"
    );
    assert_eq!(
        fs::read(path).expect("ingested file"),
        TINY_PNG,
        "bytes must survive the round trip"
    );

    wire.shutdown();

    // -- Session 2: react remove with a cold cache → /relations rebuild.
    let mut wire = Wire::spawn(&home, "run-2.log");
    wire.handshake_and_connect(&home, &bot_user);
    wire.write(serde_json::json!({
        "type": "react", "id": "ev-7",
        "chat_id": dm.room_id().as_str(),
        "message_id": hello_id.as_str(),
        "key": "👀",
        "remove": true,
    }));
    let result = wire.expect_frame("react remove result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ev-7"
    });
    assert_eq!(
        result["error"],
        serde_json::Value::Null,
        "cold-cache removal must rebuild from /relations: {result:?}"
    );

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn encrypted_group_admission_and_traffic() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e-encg"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    let bot_local = unique("encgbot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");
    let bot_id = UserId::parse(&bot_user).expect("bot id");

    let human = runtime.block_on(Human::register(&hs, &unique("encgdrew"), "human-pw"));

    let mut wire = Wire::spawn(&home, "run-1.log");
    wire.handshake_and_connect(&home, &bot_user);

    // An encrypted GROUP: named room, bot invited at creation, encryption
    // turned on immediately (the way Element's "enable encryption" works).
    let room = runtime.block_on(async {
        let room = human.open_group("war room", &bot_id).await;
        room.enable_encryption().await.expect("enable encryption");
        room
    });

    // The admission hook must fire for encrypted rooms exactly as for
    // plain ones — membership is state, never encrypted.
    let added = wire.expect_frame("chat_membership added", Duration::from_secs(30), |f| {
        f["type"] == "chat_membership" && f["change"] == "added"
    });
    assert_eq!(added["chat"]["id"], room.room_id().as_str());
    assert_eq!(added["chat"]["kind"], "group");
    assert_eq!(added["by_user_id"], human.user_id.as_str());

    // Wait until the server confirms the join AND the human's own client
    // sees the room encrypted (its send path must encrypt for the bot).
    let ready = runtime.block_on(async {
        for _ in 0..100 {
            let joined = human
                .client
                .send(joined_members::v3::Request::new(room.room_id().to_owned()))
                .await
                .map(|response| response.joined.len())
                .unwrap_or(0);
            let encrypted = room
                .latest_encryption_state()
                .await
                .map(|state| state.is_encrypted())
                .unwrap_or(false);
            if encrypted && joined >= 2 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    });
    assert!(ready, "room never became encrypted with the bot joined");

    // A mention in the encrypted group must arrive decrypted, entity intact.
    let body = format!("{bot_user} deploy the thing");
    let mention_id = runtime.block_on(human.send_mention(&room, &body, &bot_id));
    let msg = wire.expect_frame("decrypted mention", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == mention_id.as_str()
    });
    assert_eq!(msg["chat_kind"], "group");
    assert_eq!(msg["chat_title"], "war room");
    let ents = msg["entities"].as_array().expect("entities");
    assert!(
        ents.iter().any(|e| e["kind"] == "bot_mention"),
        "mention must survive decryption: {ents:?}"
    );
    // …and the server only ever saw ciphertext.
    assert_eq!(
        runtime.block_on(human.raw_event_type(&room, &mention_id)),
        "m.room.encrypted"
    );

    // The bot's reply reaches the human readable.
    wire.write(serde_json::json!({
        "type": "send", "id": "encg-1",
        "chat_id": room.room_id().as_str(),
        "text": "deploying",
    }));
    let result = wire.expect_frame("result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "encg-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let reply = result["message_id"].as_str().expect("id").to_string();
    let delivered = human.wait_for_message(&reply, Duration::from_secs(30));
    assert_eq!(delivered.body, "deploying");

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn speaker_profiles_render() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e-spk"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    let bot_local = unique("castbot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");
    let bot_id = UserId::parse(&bot_user).expect("bot id");

    // Opt into speaker:full the way an operator would — the config knob.
    let config_path = home.join("connectors/matrix/config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).expect("config")).expect("config json");
    config["speaker"] = "full".into();
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).expect("patch config");

    let human = runtime.block_on(Human::register(&hs, &unique("spkdrew"), "human-pw"));

    let mut wire = Wire::spawn(&home, "run-1.log");
    let hello = wire.handshake_and_connect(&home, &bot_user);
    assert!(
        hello["capabilities"]["features"]
            .as_array()
            .expect("features")
            .iter()
            .any(|f| f == "speaker:full"),
        "the config flag must declare the grade: {hello:?}"
    );

    let dm = runtime.block_on(human.open_dm(&bot_id));
    let hello_id = runtime.block_on(human.send_text(&dm, "who's there?"));
    wire.expect_frame("message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == hello_id.as_str()
    });

    // A cast send with an avatar: the profile rides the event, the avatar
    // is a real mxc upload.
    let avatar_path = home.join("kaiku.png");
    fs::write(&avatar_path, TINY_PNG).expect("write avatar");
    wire.write(serde_json::json!({
        "type": "send", "id": "spk-1",
        "chat_id": dm.room_id().as_str(),
        "text": "The airlock hisses open.",
        "speaker": {
            "key": "kaiku",
            "name": "Kaiku",
            "avatar_path": avatar_path.to_str().unwrap(),
        },
    }));
    let result = wire.expect_frame("cast result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "spk-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let cast_event = result["message_id"].as_str().expect("id").to_string();
    let delivered = human.wait_for_message(&cast_event, Duration::from_secs(30));
    assert_eq!(delivered.body, "The airlock hisses open.");
    let raw = runtime.block_on(human.raw_event(&dm, &cast_event));
    let profile = &raw["content"]["com.beeper.per_message_profile"];
    assert_eq!(profile["displayname"], "Kaiku", "{raw}");
    assert_eq!(profile["id"], "kaiku");
    assert!(
        profile["avatar_url"]
            .as_str()
            .is_some_and(|u| u.starts_with("mxc://")),
        "speaker:full must upload the avatar: {profile}"
    );

    // A speaker-less send stays an ordinary bot message.
    wire.write(serde_json::json!({
        "type": "send", "id": "spk-2",
        "chat_id": dm.room_id().as_str(),
        "text": "plain narrator line",
    }));
    let result = wire.expect_frame("plain result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "spk-2"
    });
    let plain_event = result["message_id"].as_str().expect("id").to_string();
    let raw = runtime.block_on(human.raw_event(&dm, &plain_event));
    assert!(
        raw["content"]["com.beeper.per_message_profile"].is_null(),
        "no speaker → no profile: {raw}"
    );

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn thread_round_trip() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e-thr"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    let bot_local = unique("thrbot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");
    let bot_id = UserId::parse(&bot_user).expect("bot id");

    let human = runtime.block_on(Human::register(&hs, &unique("thrdrew"), "human-pw"));

    let mut wire = Wire::spawn(&home, "run-1.log");
    wire.handshake_and_connect(&home, &bot_user);

    let dm = runtime.block_on(human.open_dm(&bot_id));
    let anchor_id = runtime.block_on(human.send_text(&dm, "the flaky test strikes again"));
    wire.expect_frame("anchor message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == anchor_id.as_str()
    });

    // -- thread_start anchored at the human's message: the starter threads
    // off it and the result names the derived thread chat.
    wire.write(serde_json::json!({
        "type": "thread_start", "id": "th-1",
        "chat_id": dm.room_id().as_str(),
        "from_message_id": anchor_id.as_str(),
        "name": "investigate: flaky test",
    }));
    let result = wire.expect_frame("thread_start result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "th-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let thread_chat = result["chat_id"].as_str().expect("chat_id").to_string();
    assert_eq!(
        thread_chat,
        format!("{};thread={anchor_id}", dm.room_id()),
        "derived thread chat id"
    );
    let starter = result["message_id"].as_str().expect("id").to_string();
    human.wait_for_message(&starter, Duration::from_secs(30));
    let raw = runtime.block_on(human.raw_event(&dm, &starter));
    assert_eq!(raw["content"]["m.relates_to"]["rel_type"], "m.thread");
    assert_eq!(
        raw["content"]["m.relates_to"]["event_id"],
        anchor_id.as_str()
    );

    // -- A send into the thread chat rides the thread on the wire.
    wire.write(serde_json::json!({
        "type": "send", "id": "th-2",
        "chat_id": thread_chat.as_str(),
        "text": "tracked it to a race in teardown",
    }));
    let result = wire.expect_frame("thread send result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "th-2"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let bot_thread_msg = result["message_id"].as_str().expect("id").to_string();
    human.wait_for_message(&bot_thread_msg, Duration::from_secs(30));
    let raw = runtime.block_on(human.raw_event(&dm, &bot_thread_msg));
    assert_eq!(raw["content"]["m.relates_to"]["rel_type"], "m.thread");
    assert_eq!(
        raw["content"]["m.relates_to"]["event_id"],
        anchor_id.as_str()
    );

    // -- The human's thread message routes inbound to the derived chat,
    // titled with the root snippet; the fallback is not a reply.
    let human_thread_msg =
        runtime.block_on(human.send_thread_text(&dm, &anchor_id, "nice find", None));
    let msg = wire.expect_frame("thread message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == human_thread_msg.as_str()
    });
    assert_eq!(msg["chat_id"], thread_chat.as_str());
    assert_eq!(msg["chat_kind"], "thread");
    assert_eq!(msg["chat_title"], "the flaky test strikes again");
    assert_eq!(msg["reply_to"], serde_json::Value::Null, "{msg:?}");

    // -- A real in-thread reply to the bot keeps reply_to (and, being a
    // reply to our own event, reads as a bot mention).
    let reply_id = runtime.block_on(human.send_thread_text(
        &dm,
        &anchor_id,
        "ship the fix",
        Some(&bot_thread_msg),
    ));
    let msg = wire.expect_frame("in-thread reply", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == reply_id.as_str()
    });
    assert_eq!(msg["chat_id"], thread_chat.as_str());
    assert_eq!(msg["reply_to"], bot_thread_msg.as_str());

    // -- Typing into the thread chat lands room-scoped.
    wire.write(serde_json::json!({"type": "typing", "chat_id": thread_chat.as_str()}));
    assert!(
        human.wait_for_typing(&bot_id, Duration::from_secs(15)),
        "typing into a thread chat must reach the room"
    );

    // -- Message events about thread-resident messages carry the DELIVERED
    // chat id (the host correlates on (chat_id, id)): a room-scoped frame
    // here would miss the thread's queue and, for a delete, leave the host
    // running a turn on a message the user withdrew.
    runtime.block_on(human.send_edit(&dm, &human_thread_msg, "nice find — confirmed"));
    let edited = wire.expect_frame("thread edit", Duration::from_secs(30), |f| {
        f["type"] == "message_edited" && f["id"] == human_thread_msg.as_str()
    });
    assert_eq!(edited["chat_id"], thread_chat.as_str());

    let reaction_id = runtime.block_on(human.send_reaction(&dm, &bot_thread_msg, "🎉"));
    let reaction = wire.expect_frame("thread reaction", Duration::from_secs(30), |f| {
        f["type"] == "reaction" && f["message_id"] == bot_thread_msg.as_str()
    });
    assert_eq!(reaction["chat_id"], thread_chat.as_str());

    runtime.block_on(human.redact(&dm, &reaction_id));
    let removed = wire.expect_frame("thread reaction removal", Duration::from_secs(30), |f| {
        f["type"] == "reaction" && f["removed"] == true
    });
    assert_eq!(removed["chat_id"], thread_chat.as_str());

    runtime.block_on(human.redact(&dm, &human_thread_msg));
    let deleted = wire.expect_frame("thread delete", Duration::from_secs(30), |f| {
        f["type"] == "message_deleted" && f["id"] == human_thread_msg.as_str()
    });
    assert_eq!(deleted["chat_id"], thread_chat.as_str());

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}

#[test]
#[ignore = "needs the throwaway synapse — run via `just e2e`"]
fn ask_widget_lifecycle() {
    let hs = homeserver();
    let home = std::env::temp_dir().join(unique("terva-conn-matrix-e2e-ask"));
    fs::create_dir_all(&home).expect("create TERVA_HOME");
    let runtime = Runtime::new().expect("runtime");

    let bot_local = unique("askbot");
    let bot_password = unique("pw");
    runtime.block_on(register_account(&hs, &bot_local, &bot_password));
    provision(&home, &hs, &bot_local, &bot_password);
    let bot_user = format!("@{bot_local}:localhost");
    let bot_id = UserId::parse(&bot_user).expect("bot id");

    let human = runtime.block_on(Human::register(&hs, &unique("askdrew"), "human-pw"));

    let mut wire = Wire::spawn(&home, "run-1.log");
    wire.handshake_and_connect(&home, &bot_user);

    let dm = runtime.block_on(human.open_dm(&bot_id));
    let hello_id = runtime.block_on(human.send_text(&dm, "hi"));
    wire.expect_frame("message", Duration::from_secs(30), |f| {
        f["type"] == "message" && f["id"] == hello_id.as_str()
    });

    // -- Open: the ask renders with its emoji legend and the bot seeds one
    // reaction per option on its own message.
    wire.write(serde_json::json!({
        "type": "ask", "id": "ask-1",
        "chat_id": dm.room_id().as_str(),
        "text": "Deploy to prod?",
        "options": [
            {"key": "approve", "label": "Approve", "style": "affirm", "hint": "👍"},
            {"key": "deny", "label": "Deny", "style": "deny"},
        ],
        "restrict_to": [human.user_id.as_str()],
        "expires_ms": 120000,
    }));
    let result = wire.expect_frame("ask result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ask-1"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    let ask_msg = result["message_id"].as_str().expect("id").to_string();
    let seen = human.wait_for_message(&ask_msg, Duration::from_secs(30));
    assert!(seen.body.contains("Deploy to prod?"), "got: {}", seen.body);
    assert!(seen.body.contains("👍 Approve"), "legend: {}", seen.body);
    assert_eq!(
        runtime.block_on(human.count_reactions_by(&dm, &ask_msg, &bot_user)),
        2,
        "both option seeds must be visible on the server"
    );

    // -- Answer: the restricted-to human taps a seed → attested answer.
    runtime.block_on(human.send_reaction(&dm, &ask_msg, "👍"));
    let answer = wire.expect_frame("answer", Duration::from_secs(30), |f| f["type"] == "answer");
    assert_eq!(answer["ask_id"], "ask-1");
    assert_eq!(answer["key"], "approve");
    assert_eq!(answer["user_id"], human.user_id.as_str());
    assert_eq!(answer["attestation"], "attested");

    // -- Close: seeds withdrawn, outcome rendered into the question.
    wire.write(serde_json::json!({
        "type": "ask_close", "id": "ask-2",
        "ask_id": "ask-1",
        "outcome": "Approved",
    }));
    let result = wire.expect_frame("close result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ask-2"
    });
    assert_eq!(result["error"], serde_json::Value::Null, "{result:?}");
    human.wait_for_body("the outcome edit", Duration::from_secs(30), |body| {
        body.contains("Approved")
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let seeds = runtime.block_on(human.count_reactions_by(&dm, &ask_msg, &bot_user));
        if seeds == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "seed reactions were not withdrawn on close ({seeds} left)"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    // -- Expiry: a short-lived ask withdraws its own widget; late taps are
    // plain reactions, never answers.
    wire.write(serde_json::json!({
        "type": "ask", "id": "ask-3",
        "chat_id": dm.room_id().as_str(),
        "text": "Still there?",
        "options": [{"key": "yes", "label": "Yes", "hint": "✅"}],
        "expires_ms": 2000,
    }));
    let result = wire.expect_frame("expiring ask result", Duration::from_secs(30), |f| {
        f["type"] == "result" && f["id"] == "ask-3"
    });
    let ask2_msg = result["message_id"].as_str().expect("id").to_string();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let seeds = runtime.block_on(human.count_reactions_by(&dm, &ask2_msg, &bot_user));
        if seeds == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "expiry did not withdraw the widget ({seeds} seeds left)"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    runtime.block_on(human.send_reaction(&dm, &ask2_msg, "✅"));
    let post_expiry = wire.drain_for(Duration::from_secs(5));
    assert!(
        !post_expiry.iter().any(|f| f["type"] == "answer"),
        "expired asks must not answer: {post_expiry:?}"
    );
    assert!(
        post_expiry
            .iter()
            .any(|f| f["type"] == "reaction" && f["key"] == "✅"),
        "the late tap should fall through as a plain reaction: {post_expiry:?}"
    );

    wire.shutdown();
    let _ = fs::remove_dir_all(&home);
}
