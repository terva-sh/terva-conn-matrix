# Architecture

How the design maps onto the code. Read [design.md](design.md) first
for the ideas; this document walks the process model, the modules, the
main data flows, and the invariant-carrying state, pointing into
specific types and functions. References are `file::item` — item names
are stable where line numbers are not.

## Process model

`run` is a synchronous serve loop plus a private tokio runtime, joined
by channels:

```
   stdin ──▶ reader thread ──▶ ┌────────────────┐
            (blocking read,    │   serve loop   │  main thread, sync
             detached)         │  serve.rs      │
                               └───────┬────────┘
                    commands, connect  │  Service trait
                               ┌───────▼────────┐
                               │ MatrixService  │  owns tokio Runtime
                               │ matrix/mod.rs  │  + JoinSet of tasks
                               └───────┬────────┘
             sync loop, handlers, command tasks (async)
                               ┌───────▼────────┐
   stdout ◀── writer thread ◀──│   FrameSink    │  cloneable handle
              (single writer)  └────────────────┘
```

- **The serve loop** (`serve.rs::serve_loop`) is the only consumer of
  the event channel: stdin frames, EOF, read errors, and `Fatal`
  signals all arrive there, so it can react to transport death instead
  of blocking on stdin. It dispatches through the `Service` trait —
  `MatrixService` is the production implementation; tests substitute
  their own.
- **The reader thread** (`serve.rs::read_loop`) is deliberately
  detached: it sits in a blocking read that cannot be interrupted
  after `shutdown`, and process exit reaps it. A `DeathGuard` turns a
  panicking pump thread into a `Fatal` event rather than a silent hang.
- **The writer thread** (`serve.rs::write_loop`) is the only code that
  touches stdout. `FrameSink` is the cloneable sending handle; every
  frame from every task funnels here, where `serve.rs::render` applies
  the oversize degradation (an over-cap `result` shrinks to an error
  result; anything else becomes a `warn`).
- **The runtime** lives inside `MatrixService` (`matrix/mod.rs`), so
  the serve loop never blocks on Matrix. Every background task — sync
  loop, invite reconciliation, command handlers, ask-expiry watchers —
  is spawned into one `JoinSet` for deterministic teardown.

## Startup

`main.rs::main` dispatches on the **last** argv element (the manifest
appends the lifecycle verb): `run`, or the operator verbs in
`setup.rs`. `run` installs a panic hook (panics must land in the
host-captured log with a backtrace), initializes tracing to stderr
(default filter `info,matrix_sdk=warn,matrix_sdk_crypto=warn`,
overridable via `RUST_LOG`), logs the startup banner, builds the
capability list — features never ahead of implementation, `speaker:*`
appended only when config opts in — and calls `serve.rs::serve`.

The handshake is strict (`serve.rs::wait_hello_ack`): `hello` goes out
before any Matrix work; the first inbound frame must be a `hello_ack`
carrying a protocol we declared. Then, on the host's `connect`:

1. `matrix/client.rs::connect` loads config + session and decides the
   recovery path via the marker file (`initial_sync_marker`, i.e.
   `state_dir/initial-sync-done`):
   - **marker absent** (first-ever connect): run one history-discarding
     sync with `timeout(Duration::ZERO)` (a plain `SyncSettings::new()`
     is a 30 s long-poll — it once ate the host's entire connect
     budget), write the marker, return the fresh token.
   - **marker present** (warm start): validate the session with
     `whoami()` only — no sync in the connect path — and return no
     token, so the live loop resumes from the store's persisted one.
2. `connected` goes on the wire, then `Service::on_connected` installs
   the event handlers, reconciles invites that arrived while down
   (`inbound.rs::join_pending_invites` — invites are state, not
   events), and spawns `matrix/mod.rs::sync_loop`.

`sync_loop` passes the first-connect token when there is one and
otherwise lets the SDK resume from the store. Transient failures retry
with capped exponential backoff and one `warn` per burst; an invalid
token (`is_invalid_token`: `M_UNKNOWN_TOKEN`/`M_FORBIDDEN`) or ten
consecutive failures ends the session via `FrameSink::fatal`, so the
process exits non-zero and the host's restart budget applies.

## Module map

Since 0.12.0 the protocol layer lives in the shared
[terva-sdk-rust](https://github.com/terva-sh/terva-sdk-rust) workspace and is re-exported by
`lib.rs` under the historical module names, so `serve.rs::X` references
below read as `terva-connsdk`'s `lib.rs`, `proto.rs` as
`terva-connproto`, and `wire.rs` as `terva-wire`:

```
src/
  main.rs      — verb dispatch, capabilities, tracing init
  lib.rs       — re-exports the SDK crates as proto / serve / wire
  config.rs    — Config + session persistence, on terva-env's home
                 resolution and 0600/0700 atomic writes
  setup.rs     — setup/status/reset/configured verbs
  matrix/
    mod.rs     — MatrixService, Shared, BoundedMap, sync_loop, dispatch
    client.rs  — login/restore, first-connect vs warm-resume decision
    rooms.rs   — chat identity, thread codec, delivered-scope resolution
    inbound.rs — event handlers → frames
    outbound.rs— send/edit/delete/react/thread_start/typing
    entities.rs— BotIdentity, SentCache, mention span extraction
    media.rs   — attachment ingest + send_image/send_file
    asks.rs    — AskState, widget lifecycle, try_answer
    e2ee.rs    — setup-time verification flows, BurstLimiter

terva-sdk-rust/crates/          (git-pinned by release tag since 0.12.1)
  terva-connsdk  — Service trait, serve_loop, Responder, FrameSink,
                   reader/writer threads, handshake   (was serve.rs)
  terva-connproto— frame types, envelope recovery      (was proto.rs)
  terva-wire     — FrameReader, 4 MiB cap, bounded drain (was wire.rs)
  terva-env      — terva_home, write_private, panic hook
```

## Data flow: inbound

A Matrix event becomes a frame in one hop per concern:

1. The SDK sync delivers a typed event to a handler registered in
   `inbound.rs::install_handlers`. **Handlers hold `Weak<Shared>`**:
   the handler registry lives inside the `Client`, and `Shared` holds
   that `Client` — an `Arc` here would be a cycle that keeps the
   session alive past shutdown.
2. The handler filters (not joined → drop; own sender → drop, the echo
   hygiene rule) and translates. For messages,
   `rooms.rs::inbound_shape` decides the chat identity — an `m.thread`
   relation routes to the derived `<room>;thread=<root>` chat with
   kind `thread` and a cached root-snippet title; otherwise the
   dm/group shape from `rooms.rs::chat_shape` — and records the
   event's delivered scope. `entities.rs::extract` scans for
   `bot_mention` spans (pills, `m.mentions`, reply-to-our-own via
   `SentCache`), with id-form candidates ordered before display-name
   ones (Synapse defaults display names to localparts).
3. The frame goes to `FrameSink::send` → writer thread → stdout. One
   `info!` line per delivered frame records ids, kinds, and sizes —
   never content.

Edits, redactions, and reactions ride the same path but resolve their
`chat_id` through the delivered-scope record (below) instead of
assuming the room.

## Data flow: outbound

1. The serve loop parses the command; malformed or unknown-typed
   frames with an id are answered immediately with an error result
   (envelope recovery in `proto.rs`).
2. `MatrixService::command` (`matrix/mod.rs`) clones the `Arc<Shared>`
   and spawns the handler into the `JoinSet`, moving the `Responder`
   in. The serve loop never blocks on network.
3. Handlers wrap network calls in `COMMAND_TIMEOUT` (25 s — inside the
   host's 30 s so the result stays ours to shape) and answer exactly
   once. `Responder` enforces that by construction:

   ```rust
   impl Drop for Responder {           // serve.rs
       fn drop(&mut self) {
           if let Some((id, sink)) = self.inner.take() {
               sink.send(error_result(id, "internal: command handler
                   dropped without answering".into()));
           }
       }
   }
   ```

   A cancelled or panicked task still answers, because dropping the
   responder answers.
4. `rooms.rs::resolve_chat` maps the wire `chat_id` to a `ChatTarget`
   — splitting the thread codec, or resolving a user-id chat id to the
   `m.direct` DM (`resolve_dm_with`, the cold owner-addressing path).
   Thread sends get their relation from `rooms.rs::outbound_relation`:
   explicit replies become real in-thread replies; plain sends carry
   the `is_falling_back` fallback pointed at the latest known
   in-thread event.

## Shared state and its invariants

`matrix/mod.rs::Shared` is the one state block every task sees. The
bookkeeping maps are `BoundedMap`s — insertion-ordered eviction with a
generation counter so a re-inserted key cannot be evicted by its stale
predecessor — and every consumer has a graceful fallback for an
evicted entry:

| field | cap | invariant it carries |
|---|---|---|
| `sent_ids` | 256 | our outbound event ids — a reply to one is a bot mention |
| `announced` | — | rooms whose `chat_membership added` went out; seeded with the joined set at connect so pre-session rooms are never re-announced |
| `inviters` | — | room → who invited us, recorded before any join so the membership frame can attribute |
| `seen_reactions` | 1024 | reaction event → (target, key, reactor, chat) so a redaction becomes `reaction removed`, attributed to the reactor and addressed like the delivery |
| `our_reactions` | 256 | (message, key) → our reaction event, so `react remove` is one redaction; misses rebuild from `/relations` |
| `thread_latest` | 512 | thread root → latest event, feeding the next send's reply fallback |
| `thread_scopes` | 2048 | event → delivered scope (`Some(root)` or room) — the `(chat_id, id)` correlation invariant |
| `thread_titles` | 512 | root → snippet, failures cached empty |
| `asks` | — | open widgets; `answer_events` (512) marks consumed taps so their redactions never read as deletes |
| `avatar_uris` | 64 | (speaker, path) → uploaded mxc, one upload per cast member |

The delivered-scope resolution has three entry points in `rooms.rs`:
`note_thread_event`/`note_room_event` record (every deliver/send path
calls one of them), `event_chat_id` resolves with a fetch-the-target
fallback for events from before a restart, and `cached_event_chat_id`
is the cache-only variant for redaction targets (a redacted event has
its relations stripped; misses degrade to room scope). **Any new
frame-emitting or message-sending path must use these helpers** — a
plausible-looking `room_id()` silently misses the host's queue.

## Teardown

Two ordering constraints, both learned the hard way:

- The matrix-sdk sqlite pool **aborts the process** if its destructor
  runs outside a tokio runtime. `MatrixService::Drop` therefore tears
  down inside `runtime.block_on`: abort and reap every `JoinSet` task
  (their cancelled responders queue error results), then drop the
  client, and only then let the runtime go.
- `serve.rs::serve_on` ends the session in a fixed order: drop the
  service **first** (queuing those error results), then send the
  `WriterMsg::Close` sentinel. Close — not channel closure — is what
  guarantees prompt exit: a leaked `FrameSink` clone must not be able
  to hold the wire open. Everything queued before the sentinel is
  flushed to the host.

## State on disk

`$TERVA_HOME/connectors/matrix/` (`config.rs` module docs):

```
config.json   — homeserver, user, auto_join, max_attachment_mb, speaker,
                and (0.13.0) the matrix-sdk session bundle under
                "session" — its token values SEALED in place
                (enc:age:v2, dual-recipient: our key + terva's) when the
                host has at-rest encryption on; 0600, atomic writes
secrets.key   — our own age identity (SDK-minted on the first sealed
                save; on the host's read deny-list)
store/        — matrix-sdk sqlite state + crypto stores
initial-sync-done — the first-connect marker (client.rs)
data/         — HOST-owned attachment staging; the host sweeps it
pairing.json  — HOST-owned; never read or written
```

The session moved INTO config.json because the hello now declares our
secret paths (`terva_connsdk::SealedState`, `config::SECRET_PATHS`) and
the host's per-read gate trusts that declaration — a token beside the
declared file would make it a lie by omission. A pre-0.13 session.json
is migrated in (and removed) before the hello goes out; a host with no
recipient configured gets plaintext exactly as before, converting on
the first save after `terva secret init`.

`reset` logs the device out server-side and wipes ours — config
(session included), store, marker — and never the host's two entries.

## Error posture

| situation | mechanism | outcome |
|---|---|---|
| bad credentials / unconfigured | `ConnectError::Permanent` | `connect_error`; host stops retrying |
| service down for respawn-curable reasons | `ConnectError::Fatal` / `FrameSink::fatal` | process exits non-zero; restart budget applies |
| one command fails | `Responder::err` (single logged choke point) | one readable error result |
| transient trouble worth telling the operator | `FrameSink::warn` | `warn` frame, burst-limited where repetitive (UTD) |
| outbound frame over 4 MiB | `serve.rs::render` | degraded to error result / warn — never silently skipped |
| panic anywhere | `main.rs::install_panic_hook` | backtrace in the host-captured log, non-zero exit |

## Test architecture

- **Wire conformance** (in terva-sdk-rust since 0.12.0): the mirrored
  byte-exact golden corpus and the `loop_e2e_test.go` replay run as the
  SDK crates' test suites. Locally, `tests/wire_smoke.rs` drives the
  real binary through the host conversation (including the
  malformed-command answers) and `tests/conventions.rs` guards the
  Cargo.toml/connector.json version lockstep.
- **Hermetic Matrix** (`src/matrix/tests.rs`): `MatrixMockServer`
  (wiremock) + `EventFactory` exercise every translation path. The
  test seams are `FrameSink::detached` and `Responder::detached` —
  frames land on plain mpsc channels the test asserts on.
- **Live compliance** (`tests/live_synapse.rs`, `just e2e`): eight
  scenarios drive the real binary against a throwaway Synapse
  (`testing/synapse/`, port 18008; a preconfigured Element Web on
  18009 for humans), with a second matrix-sdk client as the human —
  covering E2EE with ciphertext asserted on the wire, restart
  recovery, admission flows, asks, threads, and speaker rendering.
  Each scenario provisions throwaway accounts in a temp `TERVA_HOME`;
  nothing ever touches a real homeserver.
