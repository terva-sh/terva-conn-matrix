//! The Matrix service behind the wire: matrix-rust-sdk client, sync loop,
//! and the Matrix↔connproto translation (PLAN.md §3).

pub mod asks;
pub mod client;
pub mod e2ee;
pub mod entities;
pub mod inbound;
pub mod media;
pub mod outbound;
pub mod rooms;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
use matrix_sdk::{Client, LoopCtrl};
use tokio::runtime::Runtime;

use tokio::task::JoinSet;

use crate::config::{self, Config};
use crate::proto::{ConnectedFromConn, HostFrame, TypingFromHost};
use crate::serve::{ConnectError, FrameSink, Responder, Service, Session};

/// Give up on one outbound command well inside the host's 30 s timeout so
/// the result stays ours to shape.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);

/// Who invited us to a room — carried into the `chat_membership` frame's
/// `by_user_id`/`by_username` when the join lands.
pub(crate) struct Inviter {
    pub user_id: String,
    pub username: String,
}

/// An inbound reaction we delivered, remembered so a later redaction of its
/// event can be translated into `reaction { removed: true }` — attributed
/// to the reactor, not to whoever redacted it, and carrying the same
/// chat id the original delivery did.
pub(crate) struct SeenReaction {
    pub chat_id: String,
    pub message_id: String,
    pub key: String,
    pub user_id: String,
    pub username: String,
}

/// Session-scoped bookkeeping map, bounded so a long-running session cannot
/// grow without limit: past the cap, the oldest insertions evict first. A
/// generation counter keeps re-inserted keys safe from stale evictions;
/// both consumers have graceful fallbacks for an evicted entry.
pub(crate) struct BoundedMap<K, V> {
    map: HashMap<K, (u64, V)>,
    order: VecDeque<(u64, K)>,
    generation: u64,
    cap: usize,
}

impl<K: Eq + Hash + Clone, V> BoundedMap<K, V> {
    pub fn new(cap: usize) -> Self {
        BoundedMap {
            map: HashMap::new(),
            order: VecDeque::new(),
            generation: 0,
            cap,
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        self.generation += 1;
        self.map.insert(key.clone(), (self.generation, value));
        self.order.push_back((self.generation, key));
        while self.map.len() > self.cap {
            let Some((generation, key)) = self.order.pop_front() else {
                break;
            };
            if self.map.get(&key).is_some_and(|(g, _)| *g == generation) {
                self.map.remove(&key);
            }
        }
        // Shed leading dead order entries (removed or re-inserted keys) so
        // `order` stays proportional to the live map.
        while let Some((generation, key)) = self.order.front() {
            if self.map.get(key).is_some_and(|(g, _)| g == generation) {
                break;
            }
            self.order.pop_front();
        }
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.map.remove(key).map(|(_, value)| value)
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key).map(|(_, value)| value)
    }
}

/// Everything the async tasks share.
pub(crate) struct Shared {
    pub client: Client,
    pub config: Config,
    pub sink: FrameSink,
    /// Rooms learned to be DMs from an invite's `is_direct` flag this
    /// session — covers the gap until `m.direct` account data catches up.
    pub dm_rooms: Mutex<HashSet<OwnedRoomId>>,
    /// One unable-to-decrypt `warn` per burst, not per event.
    pub utd_warns: Mutex<e2ee::BurstLimiter>,
    /// Bot identity for mention span scanning (feature `entities`).
    pub identity: entities::BotIdentity,
    /// Our own outbound event ids — a reply to one is a bot mention.
    pub sent_ids: Mutex<entities::SentCache>,
    /// room → who invited us, recorded when the invite is seen.
    pub inviters: Mutex<HashMap<OwnedRoomId, Inviter>>,
    /// Rooms whose `chat_membership added` has been emitted. Seeded with the
    /// joined set at connect so pre-session joins are never re-announced.
    pub announced: Mutex<HashSet<OwnedRoomId>>,
    /// Host-assigned directory for inbound attachment files (from
    /// hello_ack). None = the host gave us none; attachments are skipped.
    pub data_dir: Option<PathBuf>,
    /// Inbound reactions delivered, keyed by their reaction event id — a
    /// later redaction becomes `reaction removed` instead of a delete.
    pub seen_reactions: Mutex<BoundedMap<OwnedEventId, SeenReaction>>,
    /// Our own outbound reactions: (message id, key) → our reaction event,
    /// so `react remove` can redact without a server lookup. Evicted or
    /// pre-restart entries rebuild lazily from `/relations`.
    pub our_reactions: Mutex<BoundedMap<(String, String), OwnedEventId>>,
    /// Open (and expired-but-unclosed) asks by ask id. Session-local by
    /// design — the host owns the ask lifecycle and re-asks after crashes.
    pub asks: Mutex<HashMap<String, asks::AskState>>,
    /// Reaction events consumed as ask answers (or filtered imposter taps):
    /// their redactions are widget traffic, never `message_deleted`.
    pub answer_events: Mutex<BoundedMap<OwnedEventId, ()>>,
    /// thread root → latest in-thread event, feeding the `is_falling_back`
    /// reply fallback of the next threaded send (root when unknown).
    pub thread_latest: Mutex<BoundedMap<OwnedEventId, OwnedEventId>>,
    /// event → the scope it was delivered (or sent) under: `Some(root)` for
    /// thread-chat events, `None` for room-scoped ones. The host correlates
    /// message events on `(chat_id, id)`, so an edit/delete/reaction must
    /// carry the SAME chat id its target message did — this map remembers
    /// which that was (see `rooms::event_chat_id`).
    pub thread_scopes: Mutex<BoundedMap<OwnedEventId, Option<OwnedEventId>>>,
    /// thread root → title snippet (best-effort, failures cached empty).
    pub thread_titles: Mutex<BoundedMap<OwnedEventId, String>>,
    /// (speaker key, avatar path) → uploaded mxc uri, so each cast member's
    /// avatar uploads once per session (speaker:full only).
    pub avatar_uris: Mutex<BoundedMap<(String, String), String>>,
}

impl Shared {
    pub(crate) fn new(
        client: Client,
        config: Config,
        sink: FrameSink,
        display_name: Option<String>,
        data_dir: Option<PathBuf>,
    ) -> Self {
        let user_id = client.user_id().expect("session restored");
        let identity = entities::BotIdentity {
            user_id: user_id.to_string(),
            localpart: user_id.localpart().to_string(),
            display_name,
        };
        let announced = client
            .joined_rooms()
            .into_iter()
            .map(|room| room.room_id().to_owned())
            .collect();
        Shared {
            client,
            config,
            sink,
            dm_rooms: Mutex::new(HashSet::new()),
            utd_warns: Mutex::new(e2ee::BurstLimiter::new(Duration::from_secs(60))),
            identity,
            sent_ids: Mutex::new(entities::SentCache::new()),
            inviters: Mutex::new(HashMap::new()),
            announced: Mutex::new(announced),
            data_dir,
            seen_reactions: Mutex::new(BoundedMap::new(1024)),
            our_reactions: Mutex::new(BoundedMap::new(256)),
            asks: Mutex::new(HashMap::new()),
            answer_events: Mutex::new(BoundedMap::new(512)),
            thread_latest: Mutex::new(BoundedMap::new(512)),
            thread_scopes: Mutex::new(BoundedMap::new(2048)),
            thread_titles: Mutex::new(BoundedMap::new(512)),
            avatar_uris: Mutex::new(BoundedMap::new(64)),
        }
    }
}

struct Connected {
    shared: Arc<Shared>,
    /// `Some` only on the first-ever connect — the token recorded by the
    /// one-time history-discarding sync. `None` on warm starts: the live
    /// loop resumes from the store's persisted token, delivering whatever
    /// arrived while the connector was down.
    next_batch: Option<String>,
}

/// [`Service`] implementation on the matrix-rust-sdk. Owns its tokio
/// runtime; the serve loop stays synchronous and never blocks on Matrix.
pub struct MatrixService {
    runtime: Runtime,
    state_dir: PathBuf,
    connected: Option<Connected>,
    /// Every background task (sync loop, invite joins, command handlers)
    /// lives here so teardown can reap them deterministically — see `Drop`.
    tasks: JoinSet<()>,
}

impl MatrixService {
    pub fn new() -> anyhow::Result<Self> {
        Ok(MatrixService {
            runtime: Runtime::new()?,
            state_dir: config::state_dir(),
            connected: None,
            tasks: JoinSet::new(),
        })
    }

    fn spawn(&mut self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.spawn_on(task, self.runtime.handle());
    }
}

/// The matrix-sdk sqlite pool ABORTS the process if its destructor runs
/// outside a tokio runtime context. Tear everything down inside the runtime,
/// in order: cancel + reap every task (their cancelled responders queue
/// their error results here, before serve_on's Close sentinel), then drop
/// the client while the runtime is still ambient. Only then may the runtime
/// itself go.
impl Drop for MatrixService {
    fn drop(&mut self) {
        let connected = self.connected.take();
        let mut tasks = std::mem::take(&mut self.tasks);
        self.runtime.block_on(async move {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            drop(connected);
        });
    }
}

impl Service for MatrixService {
    fn connect(
        &mut self,
        session: &Session,
        sink: &FrameSink,
    ) -> Result<ConnectedFromConn, ConnectError> {
        // Captured before `session` is shadowed by the loaded MatrixSession.
        let data_dir = (!session.data_dir.is_empty()).then(|| PathBuf::from(&session.data_dir));
        let cfg = config::load_config(&self.state_dir)
            .map_err(|err| ConnectError::Fatal(format!("cannot read config: {err}")))?;
        let Some(session) = config::load_session(&self.state_dir)
            .map_err(|err| ConnectError::Fatal(format!("cannot read session: {err}")))?
        else {
            return Err(ConnectError::Permanent(
                "not configured — run `terva bot setup --connector matrix` first".into(),
            ));
        };

        let (client, next_batch) =
            self.runtime
                .block_on(client::connect(&cfg, &self.state_dir, session))?;

        // Global profile name, for locating mention spans. Best-effort: a
        // failure only degrades bot_mention spans to the id-form candidates.
        let display_name = self.runtime.block_on(async {
            match client.account().get_display_name().await {
                Ok(name) => name,
                Err(err) => {
                    tracing::warn!("fetching own display name failed: {err}");
                    None
                }
            }
        });

        let user_id = client.user_id().expect("session restored").to_owned();
        let identity = ConnectedFromConn {
            id: user_id.to_string(),
            username: user_id.localpart().to_string(),
        };
        self.connected = Some(Connected {
            shared: Arc::new(Shared::new(
                client,
                cfg,
                sink.clone(),
                display_name,
                data_dir,
            )),
            next_batch,
        });
        Ok(identity)
    }

    fn on_connected(&mut self, _session: &Session, _sink: &FrameSink) {
        let state = self.connected.as_ref().expect("connect succeeded");
        let shared = state.shared.clone();
        let next_batch = state.next_batch.clone();
        inbound::install_handlers(shared.clone());

        // Invites that arrived while we were down are state, not events —
        // reconcile them here; new ones arrive through the handler.
        self.spawn(inbound::join_pending_invites(shared.clone()));

        self.spawn(sync_loop(shared, next_batch));
    }

    fn command(
        &mut self,
        command: HostFrame,
        _session: &Session,
        _sink: &FrameSink,
        responder: Responder,
    ) {
        let Some(state) = &self.connected else {
            responder.err("not connected");
            return;
        };
        tracing::debug!("← host: {} command", command.type_name());
        let shared = state.shared.clone();
        match command {
            HostFrame::Send(send) => {
                self.spawn(outbound::send(shared, send, responder));
            }
            HostFrame::SendImage(send) => {
                self.spawn(media::send_image(shared, send, responder));
            }
            HostFrame::SendFile(send) => {
                self.spawn(media::send_file(shared, send, responder));
            }
            HostFrame::Edit(edit) => {
                self.spawn(outbound::edit(shared, edit, responder));
            }
            HostFrame::React(react) => {
                self.spawn(outbound::react(shared, react, responder));
            }
            HostFrame::Delete(delete) => {
                self.spawn(outbound::delete(shared, delete, responder));
            }
            HostFrame::Ask(ask) => {
                // The expiry watcher lives in the JoinSet like every task
                // holding Shared (teardown discipline); expire() tolerates
                // firing before the ask registers or after it closes.
                if ask.expires_ms > 0 {
                    let shared = shared.clone();
                    let ask_id = ask.id.clone();
                    let wait = Duration::from_millis(ask.expires_ms.max(1000) as u64);
                    self.spawn(async move {
                        tokio::time::sleep(wait).await;
                        asks::expire(shared, ask_id).await;
                    });
                }
                self.spawn(asks::handle_ask(shared, ask, responder));
            }
            HostFrame::AskClose(close) => {
                self.spawn(asks::handle_ask_close(shared, close, responder));
            }
            HostFrame::ThreadStart(start) => {
                self.spawn(outbound::thread_start(shared, start, responder));
            }
            // The serve loop never forwards these as commands.
            HostFrame::HelloAck(_)
            | HostFrame::Connect
            | HostFrame::Typing(_)
            | HostFrame::Shutdown => {
                responder.err("internal: non-command frame dispatched as command");
            }
        }
    }

    fn typing(&mut self, typing: TypingFromHost, _session: &Session, _sink: &FrameSink) {
        if let Some(state) = &self.connected {
            let shared = state.shared.clone();
            // Absent `active` is the pulse; `Some(false)` is the stop the
            // host sends once after each reply because we declared
            // `typing_stop` — it is what keeps "…is typing" from lingering
            // beside the delivered answer for the rest of our 30 s PUT.
            let active = typing.active.unwrap_or(true);
            self.spawn(outbound::typing(shared, typing.chat_id, active));
        }
    }
}

/// The live sync loop: transient failures retry with backoff and surface one
/// `warn` per burst; an invalid token (or a hopeless burst) ends the session
/// fatally so the host's restart budget applies.
///
/// With no explicit token the SDK resumes from the token the store
/// persisted last session — the recovery path for messages that arrived
/// while the connector was down.
async fn sync_loop(shared: Arc<Shared>, next_batch: Option<String>) {
    const MAX_CONSECUTIVE_FAILURES: u32 = 10;
    let mut settings = SyncSettings::new().timeout(Duration::from_secs(30));
    let from = if let Some(token) = next_batch {
        settings = settings.token(token);
        "the first-connect token (history discarded)"
    } else {
        "the store's persisted token (downtime recovered)"
    };
    let failures = Arc::new(AtomicU32::new(0));
    let client = shared.client.clone();

    tracing::info!("live sync loop running from {from}");
    let result = client
        .sync_with_result_callback(settings, |result| {
            let shared = shared.clone();
            let failures = failures.clone();
            async move {
                use std::sync::atomic::Ordering;
                match result {
                    Ok(_) => {
                        failures.store(0, Ordering::Relaxed);
                        Ok(LoopCtrl::Continue)
                    }
                    Err(err) => {
                        if is_invalid_token(&err) {
                            shared.sink.fatal(format!(
                                "homeserver rejected our access token mid-session (M_UNKNOWN_TOKEN): {err}"
                            ));
                            return Ok(LoopCtrl::Break);
                        }
                        let n = failures.fetch_add(1, Ordering::Relaxed) + 1;
                        if n == 1 {
                            // One warn per burst, not per failure.
                            shared
                                .sink
                                .warn(format!("matrix sync failed (retrying): {err}"));
                        }
                        tracing::warn!(attempt = n, "sync failed: {err}");
                        if n >= MAX_CONSECUTIVE_FAILURES {
                            shared.sink.fatal(format!(
                                "matrix sync failed {n} times in a row; giving up: {err}"
                            ));
                            return Ok(LoopCtrl::Break);
                        }
                        tokio::time::sleep(backoff(n)).await;
                        Ok(LoopCtrl::Continue)
                    }
                }
            }
        })
        .await;

    if let Err(err) = result {
        shared.sink.fatal(format!("matrix sync loop ended: {err}"));
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(6).saturating_sub(1))
}

/// M_UNKNOWN_TOKEN / M_FORBIDDEN — the session is dead, not the network.
pub(crate) fn is_invalid_token(err: &matrix_sdk::Error) -> bool {
    matches!(
        err.client_api_error_kind(),
        Some(ErrorKind::UnknownToken { .. }) | Some(ErrorKind::Forbidden)
    )
}
