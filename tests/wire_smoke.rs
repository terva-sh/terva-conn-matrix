//! Wire smoke tests: drive the real binary through the conversations the
//! terva host would have with it — the Rust analog of terva's
//! `chat/external/proxy_test.go` helper-process tests.

use std::io::{BufRead, BufReader, Lines, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

const HELLO_ACK: &[u8] =
    b"{\"type\":\"hello_ack\",\"protocol\":2,\"terva_version\":\"test\",\"data_dir\":\"/tmp/none\"}\n";

struct Connector {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Connector {
    fn spawn() -> Self {
        // Point TERVA_HOME at an empty per-test dir: the smoke conversation
        // must see an unconfigured connector even on machines where a real
        // Matrix session exists.
        let home =
            std::env::temp_dir().join(format!("terva-conn-matrix-smoke-{}", std::process::id()));
        Self::spawn_in_home(&home)
    }

    fn spawn_in_home(home: &std::path::Path) -> Self {
        std::fs::create_dir_all(home).expect("create test TERVA_HOME");
        let mut child = Command::new(env!("CARGO_BIN_EXE_terva-conn-matrix"))
            .arg("run")
            .env("TERVA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn connector");
        let stdin = Some(child.stdin.take().expect("stdin"));
        let lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        Connector {
            child,
            stdin,
            lines,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.stdin
            .as_mut()
            .expect("stdin open")
            .write_all(bytes)
            .expect("write to connector");
    }

    fn read_frame(&mut self) -> serde_json::Value {
        let line = self
            .lines
            .next()
            .expect("a frame before EOF")
            .expect("read frame line");
        serde_json::from_str(&line).expect("frame is JSON")
    }

    /// Read the `hello` frame and answer the protocol-2 `hello_ack`.
    fn handshake(&mut self) {
        let hello = self.read_frame();
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["name"], "matrix");
        assert_eq!(hello["protocol_min"], 2);
        assert_eq!(hello["protocol_max"], 2);
        // P2 declarations (message_ids/chat_kinds are informative), the P4
        // group-admission features, and the P5 message events + attachments.
        assert_eq!(hello["capabilities"]["max_text_len"], 24000);
        assert_eq!(hello["capabilities"]["typing_refresh_ms"], 20000);
        assert_eq!(hello["capabilities"]["sends_images"], true);
        assert_eq!(hello["capabilities"]["sends_files"], true);
        assert_eq!(hello["capabilities"]["min_edit_interval_ms"], 1000);
        let features = hello["capabilities"]["features"]
            .as_array()
            .expect("features");
        for feature in [
            "message_ids",
            "chat_kinds",
            "entities",
            "chat_membership",
            "edits_in",
            "edits_out",
            "deletes_in",
            "deletes_out",
            "reactions_in",
            "reactions_out",
            "attachment_kinds",
            "asks",
            "threads_out",
            "typing_stop",
        ] {
            assert!(
                features.iter().any(|f| f == feature),
                "missing declared feature {feature}"
            );
        }
        self.write(HELLO_ACK);
    }

    /// Close stdin and wait for exit.
    fn finish(mut self) -> (std::process::ExitStatus, bool) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait");
        let more_frames = self.lines.next().is_some();
        (status, more_frames)
    }
}

#[test]
fn handshake_connect_error_and_clean_shutdown() {
    let mut conn = Connector::spawn();
    conn.handshake();
    conn.write(b"{\"type\":\"connect\"}\n");

    // Unconfigured refuses connect with a clear, permanent error — and the
    // process keeps serving (never exits over an unconfigured connect).
    let connect_err = conn.read_frame();
    assert_eq!(connect_err["type"], "connect_error");
    let msg = connect_err["error"].as_str().expect("error text");
    assert!(
        msg.contains("not configured"),
        "unexpected error text: {msg}"
    );

    // typing owes no result — the pulse and the stop alike; the next
    // id-carrying command still gets exactly one, so the first frame after
    // all three is the send's result.
    conn.write(b"{\"type\":\"typing\",\"chat_id\":\"c1\"}\n");
    conn.write(b"{\"type\":\"typing\",\"chat_id\":\"c1\",\"active\":false}\n");
    conn.write(b"{\"type\":\"send\",\"id\":\"t-1\",\"chat_id\":\"c1\",\"text\":\"hi\"}\n");
    let result = conn.read_frame();
    assert_eq!(result["type"], "result");
    assert_eq!(result["id"], "t-1");
    assert!(result["error"].as_str().is_some_and(|e| !e.is_empty()));

    // Unknown id-less frame types are tolerated, and shutdown exits 0.
    conn.write(b"{\"type\":\"totally_new_frame\"}\n{\"type\":\"shutdown\"}\n");
    let (status, more) = conn.finish();
    assert!(status.success(), "connector exited {status:?}");
    assert!(!more, "no frames after shutdown");
}

#[test]
fn hello_arrives_within_the_host_deadline() {
    // The host kills a child that emits no hello within 3 s of spawn.
    let start = Instant::now();
    let mut conn = Connector::spawn();
    let hello = conn.read_frame();
    assert_eq!(hello["type"], "hello");
    assert!(
        start.elapsed().as_secs_f64() < 3.0,
        "hello took {:?}",
        start.elapsed()
    );
    let (status, _) = conn.finish();
    assert!(status.success());
}

#[test]
fn stdin_eof_is_a_clean_shutdown() {
    // Close stdin immediately after the (unread) hello — the host does this
    // when it dies; the connector must exit promptly and cleanly.
    let conn = Connector::spawn();
    let (status, _) = conn.finish();
    assert!(status.success(), "connector exited {status:?}");
}

#[test]
fn non_hello_ack_first_frame_is_fatal() {
    // A confused host is a handshake failure, not something to limp past.
    let mut conn = Connector::spawn();
    let hello = conn.read_frame();
    assert_eq!(hello["type"], "hello");
    conn.write(b"{\"type\":\"connect\"}\n");
    let (status, _) = conn.finish();
    assert!(!status.success(), "expected non-zero exit, got {status:?}");
}

#[test]
fn out_of_range_protocol_ack_is_fatal() {
    // Unreachable with a correct host (it refuses before acking outside our
    // declared range) — but we never continue on an unagreed protocol.
    let mut conn = Connector::spawn();
    let hello = conn.read_frame();
    assert_eq!(hello["type"], "hello");
    conn.write(b"{\"type\":\"hello_ack\",\"protocol\":1,\"terva_version\":\"old\"}\n");
    let (status, _) = conn.finish();
    assert!(!status.success(), "expected non-zero exit, got {status:?}");
}

#[test]
fn unknown_command_with_id_gets_an_error_result() {
    // A future host command must not starve its 30 s timeout on us.
    let mut conn = Connector::spawn();
    conn.handshake();
    conn.write(b"{\"type\":\"broadcast\",\"id\":\"f-1\",\"chat_id\":\"c1\"}\n");
    let result = conn.read_frame();
    assert_eq!(result["type"], "result");
    assert_eq!(result["id"], "f-1");
    assert!(result["error"].as_str().is_some_and(|e| !e.is_empty()));
    conn.write(b"{\"type\":\"shutdown\"}\n");
    let (status, _) = conn.finish();
    assert!(status.success());
}

#[test]
fn malformed_command_body_still_gets_its_result() {
    // chat_id has the wrong type: the body fails to decode but the id is
    // recovered from the envelope (the Go SDK silently drops these).
    let mut conn = Connector::spawn();
    conn.handshake();
    conn.write(b"{\"type\":\"send\",\"id\":\"m-1\",\"chat_id\":42,\"text\":\"hi\"}\n");
    let result = conn.read_frame();
    assert_eq!(result["type"], "result");
    assert_eq!(result["id"], "m-1");
    assert!(result["error"].as_str().is_some_and(|e| !e.is_empty()));
    conn.write(b"{\"type\":\"shutdown\"}\n");
    let (status, _) = conn.finish();
    assert!(status.success());
}

#[test]
fn null_vec_fields_are_not_malformed() {
    // Go marshals nil slices as null; the frame must decode, reach the
    // service, and get a normal (seed-error) result — not be dropped.
    let mut conn = Connector::spawn();
    conn.handshake();
    conn.write(
        b"{\"type\":\"ask\",\"id\":\"a-1\",\"chat_id\":\"c1\",\"text\":\"t\",\"options\":null,\"restrict_to\":null}\n",
    );
    let result = conn.read_frame();
    assert_eq!(result["type"], "result");
    assert_eq!(result["id"], "a-1");
    conn.write(b"{\"type\":\"shutdown\"}\n");
    let (status, _) = conn.finish();
    assert!(status.success());
}

#[test]
fn oversized_inbound_frame_is_skipped_and_the_stream_recovers() {
    let mut conn = Connector::spawn();
    conn.handshake();
    // 5 MiB of junk on one line — over the 4 MiB cap. Skipped with a stderr
    // warning; the stream (and the send after it) must survive.
    let mut oversized = vec![b'x'; 5 << 20];
    oversized.push(b'\n');
    conn.write(&oversized);
    conn.write(b"{\"type\":\"send\",\"id\":\"o-1\",\"chat_id\":\"c1\",\"text\":\"hi\"}\n");
    let result = conn.read_frame();
    assert_eq!(result["type"], "result");
    assert_eq!(result["id"], "o-1");
    conn.write(b"{\"type\":\"shutdown\"}\n");
    let (status, _) = conn.finish();
    assert!(status.success());
}

#[test]
fn hello_declares_sealed_secrets_once_a_key_exists() {
    // An unconfigured connector (no key of its own) must stay silent —
    // announcing a recipient it does not have would register a component
    // terva could never re-seal to.
    let bare_home = std::env::temp_dir().join(format!(
        "terva-conn-matrix-smoke-bare-{}",
        std::process::id()
    ));
    let mut conn = Connector::spawn_in_home(&bare_home);
    let hello = conn.read_frame();
    assert_eq!(hello["type"], "hello");
    assert!(
        hello.get("secrets").is_none(),
        "no key yet, so no declaration: {hello}"
    );
    let (status, _) = conn.finish();
    assert!(status.success());

    // A host with at-rest encryption + a configured session: the sealed
    // save mints our key, and the next hello carries the declaration the
    // host's recipient registry and per-read gate feed on.
    let home = std::env::temp_dir().join(format!(
        "terva-conn-matrix-smoke-sealed-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let state_dir = home.join("connectors").join("matrix");
    std::fs::create_dir_all(&state_dir).unwrap();
    let terva_id = age::x25519::Identity::generate();
    std::fs::write(
        home.join("config.json"),
        format!(
            "{{\"secrets\":{{\"recipient\":\"{}\"}}}}\n",
            terva_id.to_public()
        ),
    )
    .unwrap();
    let session: matrix_sdk::authentication::matrix::MatrixSession =
        serde_json::from_value(serde_json::json!({
            "user_id": "@bot:example.org",
            "device_id": "SMOKEDEV",
            "access_token": "syt_smoke_token",
        }))
        .unwrap();
    let config = terva_conn_matrix::config::Config {
        homeserver_url: "https://matrix.example.org".into(),
        session: Some(session),
        ..Default::default()
    };
    terva_conn_matrix::config::save_config(&state_dir, &config).unwrap();

    let mut conn = Connector::spawn_in_home(&home);
    let hello = conn.read_frame();
    assert_eq!(hello["type"], "hello");
    let secrets = hello
        .get("secrets")
        .unwrap_or_else(|| panic!("hello must declare sealed state: {hello}"));
    assert!(
        secrets["recipient"]
            .as_str()
            .is_some_and(|r| r.starts_with("age1")),
        "declared recipient: {secrets}"
    );
    assert_eq!(
        secrets["paths"],
        serde_json::json!(["/session/access_token", "/session/refresh_token"]),
    );
    let (status, _) = conn.finish();
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&bare_home);
}
