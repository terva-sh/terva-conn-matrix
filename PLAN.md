# terva-conn-matrix — implementation plan

A **Matrix chat connector for terva**, written in Rust on the native
[matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk), speaking the
**connector protocol (connproto)** as a standalone external process. This is
Phase 6 of terva's `docs/plans/archive/chat-connectors.md`, which explicitly prefers an
external, out-of-process Matrix connector built against the wire — this repo is
that connector.

Status: **P8 landed** (polish + release — see CHANGELOG.md). All eight
phases are implemented: the full feature surface is declared
(`message_ids, chat_kinds, entities, chat_membership, edits_in/out,
deletes_in/out, reactions_in/out, attachment_kinds, asks, threads_out,
typing_stop`, plus config-gated `speaker:*`), `just dist` produces linkable prebuilt
archives, and the README documents real-homeserver setup. What remains
is operator-side: the graduation dogfood against a real homeserver,
off the full-surface walkthrough in `testing/DOGFOOD.md` (DM, E2EE,
groups, message events, attachments, asks, threads, recovery). The
MSC3381 poll widget stays future enhancement.

---

## 1. Ground rules (from the connproto contract)

The authoritative contract lives in the terva repo — read these before
touching the wire (paths relative to `terva-sh/terva`):

1. `packages/agent/connproto/connproto.go` — the complete frame schema, heavily
   commented (~467 lines).
2. `packages/agent/connproto/connproto_test.go` (`TestGoldenFrames`,
   `TestGoldenDecode`) — **byte-exact golden frames; our conformance oracle**.
3. `docs/connectors.md` §"Protocol reference" — the prose contract and rules.
4. `packages/agent/connsdk/connsdk.go` (`Serve`) — the Go author-side state
   machine we are re-implementing in Rust.
5. `packages/agent/chat/connhost/connhost.go` — the host we must satisfy.
6. `packages/agent/chat/external/proxy.go` — spawn, timeouts, restart budget,
   shutdown escalation.
7. `packages/agent/chat/extconn/loop_e2e_test.go` — a working sh-script
   connector; the smallest foreign-language reference.

The load-bearing facts:

- **Transport**: terva spawns the manifest's `exec` with the lifecycle verb
  appended as the **last** argv element. `run` speaks the protocol:
  host→connector on **stdin**, connector→host on **stdout**, one JSON object
  per LF-terminated line. **stderr is free-form logging** (captured to
  `$TERVA_HOME/logs/connector-matrix.log`). One stray stdout byte corrupts the
  wire.
- **Frame cap**: 4 MiB per line (`connproto.MaxFrameBytes`). Over-limit frames
  are skipped with a warning by the peer, not fatal — but a skipped frame is a
  *silently lost* frame, so never produce one.
- **Handshake**: the **connector speaks first** — `hello` must hit stdout
  within **3 seconds** of spawn (`defaultHelloTimeout`). Therefore: emit
  `hello` before *any* Matrix work; never block startup on network. Negotiated
  version = `min(host_max, our_max)`; host refuses if our `protocol_min`
  exceeds it. We declare `protocol_min: 2, protocol_max: 2` — v1's
  id-in-reply_to conflation is not worth shimming for hosts older than terva
  v0.114 (and `RequireProtocol` floors are sanctioned when a wire feature is
  genuinely load-bearing; v2 message identity is).
- **Correlation**: every host command (`send`, `send_image`, `send_file`,
  `ask`, `ask_close`, `edit`, `react`, `delete`, `thread_start`) carries an
  `id` and gets **exactly one** `result` echoing it (`error` set on failure;
  `message_id`/`chat_id` on success where applicable). `typing` is
  fire-and-forget. `connect` is answered by exactly one `connected` or
  `connect_error`. `answer` frames flow free, keyed by `ask_id`.
- **Timeouts** (host side): connect 30 s, per-send 30 s, ask default 2 min.
  Crash budget: 3 restarts / 60 s with doubling backoff, then the bridge is
  declared broken. `connect_error` means *permanently* broken (bad creds) — do
  not use it for transient network trouble; retry internally and surface a
  `warn` instead. A fatal transport death should exit the process promptly so
  the restart budget applies.
- **Shutdown**: on `{"type":"shutdown"}` **or stdin EOF**, exit promptly. The
  host escalates stdin-close → 2 s → SIGTERM → 1 s → SIGKILL.
- **Features are declared, never assumed**: `hello.capabilities.features`
  lists what we *produce*; `hello_ack.capabilities.features` lists what the
  host *consumes*. Only emit an optional construct the peer declared. Current
  host consume list: `message_ids, chat_kinds, asks, entities,
  chat_membership, edits_in, deletes_in, reactions_in, attachment_kinds`.
- **Paths**: `hello_ack.data_dir` (= `$TERVA_HOME/connectors/matrix/data`) is
  where inbound attachments are written — the host **evalsymlinks-contains
  checks, then reads-and-deletes (images) or moves (other kinds)** everything
  we reference there. Credentials and the SDK store therefore live in the
  connector state dir `$TERVA_HOME/connectors/matrix/` (a sibling, *outside*
  `data`), same split as `connsdk.StateDir`.
- **Transport, not policy**: pairing, group admission, mention gating,
  approvals, queueing, chunking to `max_text_len`, and built-in commands are
  all host-side. We normalize Matrix ↔ frames and render widgets; we never
  decide who may talk to the agent.
- **Echo hygiene**: the host drops edits/deletes on its own recent
  `result.message_id`s, but we must still not re-deliver the bot's own
  messages inbound (filter `sender == own user_id` at the sync layer).

## 2. Manifest, verbs, install

`connector.json` (in-repo, seeded):

```json
{
  "name": "matrix",
  "version": "0.1.0",
  "exec": "./run.sh",
  "args": [],
  "enabled": true,
  "description": "Matrix chat connector (matrix-rust-sdk, E2EE-capable)"
}
```

- `matrix` has no compiled-in namesake (only `telegram`/`discord` are
  built-in, and terva's plan wants Matrix external), so the clean name is
  ours. Discovery requires the install dir basename to equal the manifest
  name: `$TERVA_HOME/connectors/matrix/connector.json`.
- Install routes: `terva bot link $(pwd)/connector.json` (dev symlink),
  a straight copy of the tree, or `terva --connector-manifest …` for one run.
- `run.sh` mirrors `terva-ext-index`'s launcher: `needs_build()` →
  `cargo build --release --locked` → `exec target/release/terva-conn-matrix
  "$@"` — all chatter on stderr, verb passed through untouched. (The
  prebuilt-download fallback from ext-index is release-tier work, phase 8.)

Verbs (dispatch on `argv[last]`):

| verb | behavior |
|---|---|
| `run` | the bridge session (wire on stdio) |
| `setup` | interactive login on the inherited tty: homeserver URL → auth (password or SSO) → device name `terva` → persist session; then optional E2EE verification/recovery (phase 3) |
| `status` | one config block on stdout, 5 s budget, **tokens masked** (print shape, not values) |
| `reset` | log out the device (invalidate token server-side), wipe state dir |
| `configured` | exit 0 iff a restorable session exists (5 s budget, no network) |

## 3. Matrix ↔ connproto mapping

The heart of the connector. Left column is the wire construct, right is the
Matrix realization via matrix-sdk / ruma.

### Identity & chats

| connproto | Matrix |
|---|---|
| `connected.id` | our full MXID (`@bot:example.org`) |
| `connected.username` | localpart (stable, unlike displayname) |
| `chat_id` | room id (`!abc:example.org`); threads get a derived id, §threads |
| `chat_kind` | `dm` when the room is in `m.direct` account data **or** is a 2-member room created with `is_direct`; `group` otherwise; `thread` for thread chats. Matrix has no strong `channel` analog — unused. |
| `chat_title` | room display name (SDK-computed); empty for DMs |
| `scope_id` | leave empty initially; a parent Space id is the natural candidate if scoping is ever wanted |
| `message.id` | event id (`$abc…`) |
| `message.ts` | `origin_server_ts` (already ms) |
| `reply_to` | `m.relates_to.m.in_reply_to.event_id` (rich replies) |
| `user_id` / `username` | sender MXID / sender localpart (displaynames are per-room and mutable; localpart is the honest stable handle) |

### Text

- Inbound: `content.body` (the plain-text projection every event carries).
  Strip the `> ` fallback quote block from rich replies (the SDK exposes
  helpers).
- Outbound: render markdown → `formatted_body` (`org.matrix.custom.html`) via
  the SDK's `markdown` feature, `body` = the plain source. terva's replies are
  markdown-shaped, and Matrix is one of the few services that renders it
  properly.
- `max_text_len`: a whole Matrix event (PDU) is capped at 65535 bytes and
  `body` + `formatted_body` both carry the content, so budget ≈ half minus
  envelope. Declare **24000** to start; tune with real traffic.
- `typing`: `PUT …/typing` with a 30 s server timeout; declare
  `typing_refresh_ms: 20000` so the host re-asserts before expiry. We
  declare `typing_stop`, so the host also sends one `typing` with
  `active:false` after each reply; that PUTs `typing: false`, clearing
  the indicator at once instead of letting the 30 s run out.

### Entities (feature `entities`)

- `bot_mention`: inbound `m.mentions.user_ids` containing our MXID (stable
  since Matrix v1.7 — exactly what mention-gated group admission needs).
  Locate the span by scanning `body` for our display name / localpart pill
  text; emit `offset:0,length:0` ("present, unlocatable") when the scan
  misses. Also honor legacy pills (`matrix.to` anchors in `formatted_body`)
  and reply-to-bot as mention (the host gate treats only `bot_mention` as the
  gate key).
- `mention`/`link`/`code` for other users' pills, anchors, and `<code>` spans
  are nice-to-have; offsets are **Unicode code points** over `text`.

### Edits / deletes / reactions (features `edits_in/out`, `deletes_in/out`, `reactions_in/out`)

| wire | Matrix |
|---|---|
| in `message_edited` | `m.replace` relation → deliver `m.new_content.body` under the **original** event id |
| out `edit` | send `m.room.message` with `m.new_content` + `m.replace` rel to `message_id` |
| in `message_deleted` | `m.room.redaction` of a message event |
| out `delete` | `PUT …/redact` on `message_id` |
| in `reaction` | `m.reaction` annotation; `key` = the annotation key verbatim (emoji or arbitrary string — connproto keys are already opaque) |
| out `react` | send `m.reaction` (`m.annotation` rel); `remove` = redact **our own** reaction event for that key (keep a key→our-reaction-event-id map, and rebuild it lazily from `/relations` after a restart) |
| reaction `removed` | redaction of a reaction event → look up its key via the relations aggregation |
| `min_edit_interval_ms` | declare **1000** — homeserver rate limits (429 + `retry_after_ms`) are per-txn and edits are the spammy path |

**Chat-id correlation invariant (the 0.10.1 rule):** the host matches
message events to queued prompts on the compound key **`(chat_id, id)`**
(verified against the host source — `loop.go` queue rewrites; see
docs/connproto-proposals-host-review.md). So every `message_edited`,
`message_deleted`, and `reaction` frame must carry the chat id its target
message was **delivered** under — for a thread-resident message that is
the derived thread chat, not the room, even though the Matrix edit/
redaction/reaction event never re-states its thread. Mechanism: every
delivery and send path records its event's scope
(`rooms::note_thread_event` / `note_room_event` → the bounded
`thread_scopes` cache); emitters resolve via `rooms::event_chat_id`
(cache, then fetch-the-target fallback for pre-restart messages) or
`cached_event_chat_id` (redaction targets — a redacted event has its
relations stripped, so cache-only, degrading to room-scoped). **Any new
frame-emitting or message-sending path must call the same helpers** —
a frame stamped with a merely-plausible chat id silently misses the
host's queue (the sharp case: a deleted thread message still runs as an
agent turn).

### Attachments (feature `attachment_kinds`)

Inbound (`message.attachments[]`): download (and **decrypt** — the SDK does
both in one call) into `data_dir/`, then reference the path.

| Matrix msgtype/event | `kind` |
|---|---|
| `m.image` | `image` (host inlines to the model and deletes the file) |
| `m.audio` | `audio`; with the voice flag (`org.matrix.msc3245.voice` / `m.voice`) → `voice` |
| `m.video` | `video` |
| `m.file` | `document` |
| `m.sticker` event | `sticker` |

`name` = `filename` (or `body` when it's a filename), `size`/`duration_ms`
from `info`, `caption` = `body` when distinct from `filename` (Matrix v1.10
caption semantics). Enforce a size ceiling (skip + `warn`) so a 2 GB video
never lands in `data_dir`.

Outbound: `send_image`/`send_file` → upload (encrypting in E2EE rooms —
`Room::send_attachment` handles it) → `m.image`/`m.file` with caption.
Declare `sends_images: true, sends_files: true`.

### Membership (feature `chat_membership`)

- Room **invite** → auto-join (config-gated, §5) → `chat_membership{change:
  "added", by_user_id: inviter}`. The host then runs its admission flow (owner
  DM ask / `/approve`); we never gate.
- Our own leave/kick/ban → `change:"removed"` (host auto-revokes admission).
- The bot's own membership is what's tracked — not other members' churn.

### Asks (feature `asks`) — reaction-widget rendering

Matrix has no buttons. Two candidate widgets, staged:

1. **Pre-seeded reactions** (phase 6, the shipped default): send the ask text
   (options numbered, with their `hint` emoji when present), then react to our
   own ask message with each option's emoji (hint, else ①②③… fallbacks).
   A user tapping a reaction sends `m.reaction` with an authenticated
   `sender` → map annotation key → option key → `answer`. `ask_close` →
   redact our seed reactions + edit the ask message to the outcome text.
2. **MSC3381 polls** (optional, later): poll-start/response events; Element
   renders them natively. Still unstable-prefixed in the wild; the reaction
   widget works everywhere, so polls are an enhancement, not the floor.

Attestation: a Matrix reaction/poll response is a signed event whose `sender`
is authenticated by the origin homeserver — grade `attested`, with one honest
caveat: a malicious *federated* homeserver can forge events only for **its
own** users, and an MXID embeds its server, so exact-MXID `restrict_to`
matching (which the host re-checks anyway) keeps grants sound unless the
owner's own homeserver is hostile. Document; don't over-engineer.

Edge rules from the contract: re-filter `restrict_to` service-side but rely on
the host's re-check; `expires_ms` honored by closing our widget; multi-select
and custom-answer asks never reach us (the host routes those to its text
floor); asks never carry a speaker.

### Threads (feature `threads_out`)

Matrix threads (stable since v1.4) are per-message relation trees inside a
room — not first-class chats. Mapping decision:

- `thread_start{chat_id, from_message_id, name}` → send a starter message
  (the `name`, as text) with an `m.thread` rel to `from_message_id` (when
  anchorless: post an anchor first, then thread off it). Result
  `chat_id` = **`<room_id>;thread=<root_event_id>`** — a derived id we parse
  on every outbound command (`;thread=` splits it; sends into such a chat get
  the `m.thread` relation with `is_falling_back` reply fallback).
- Inbound events carrying an `m.thread` rel → `chat_kind: "thread"`,
  `chat_id` = the derived id, `chat_title` = best-effort thread root snippet.
- Message events (edits/deletes/reactions) about a thread-resident message
  carry the derived id too — the chat-id correlation invariant in
  §edits/deletes/reactions applies to threads with full force, because the
  underlying Matrix events never re-state their thread.
- The derived-id scheme is **our** convention riding connproto's opaque
  chat ids — document it in README so nothing else tries to parse room ids.

### Speaker (deferred to phase 8)

Do **not** declare `speaker:*` at first: an undeclared speaker makes the host
render its own `**Name:** ` prefix fallback, which works in every Matrix
client. Phase 8 upgrades to `speaker:name_only`/`speaker:full` via **MSC4144
per-message profiles** (`m.per_message_profile` / unstable
`com.beeper.per_message_profile`, already rendered by several clients and used
by mautrix bridges) with avatar upload to `mxc://` for `full`. Gate on a
config flag until rendering support is common enough; when the flag is off we
simply don't declare the feature and the host fallback does the work.

### E2EE (phase 3, the Matrix-specific hard part)

- SDK `e2e-encryption` + `sqlite` store in the **state dir** (never
  `data_dir`). Encrypted rooms then mostly Just Work: the SDK
  encrypts/decrypts events and attachments transparently.
- `setup` gets a verification story: after login, offer (a) recovery-key
  restore (server-side key backup + cross-signing → the device becomes
  verified and can read history), or (b) interactive emoji verification
  (SAS) from another logged-in client, driven on the tty. Without either the
  bot still works in encrypted rooms going forward but shows as unverified.
- Unable-to-decrypt (UTD) events → `warn` frame once per burst, not per event.
- `reset` must invalidate the device server-side (`logout`), then wipe the
  store — a deleted-but-live token is a leak.

## 4. Sync-loop discipline

- **Discard history once, recover downtime always.** The FIRST-ever connect
  runs one initial sync and *discards* its timeline (a freshly set-up bot
  must not replay pre-setup room history into agent turns); a marker file
  records that it ran. Every later connect resumes the live loop from the
  token the store persisted, so messages that arrived while the connector
  was down are DELIVERED on reconnect — the dogfood found that silently
  eating them reads as a broken bridge, not discipline. Long outages are
  bounded by the server's limited-timeline window (the most recent events
  per room arrive, not the whole gap; and the host queues only a few
  hundred normalized messages between the wire and its agent loop —
  overflow past that is dropped with a warn into the host's log, so
  pacing a large recovery burst is ours to do). That queue holds
  `message` frames ONLY: results, answers, membership, and the
  edit/delete/reaction streams are dispatched directly and never
  contend for it (doc-proposals item 8 proposes documenting this
  host-side).
- Filter own-sender events (echo hygiene) and any event without a room.
- Transient sync failure → internal retry with backoff + one `warn`;
  permanent auth failure (`M_UNKNOWN_TOKEN`) → exit non-zero (the host's
  restart budget + `connect_error` on respawn surface it) after logging
  clearly to stderr.
- Order guarantee: frames for one chat must be emitted in Matrix timeline
  order — a single outbound writer task (mpsc-serialized) gives a total order
  for free.

## 5. Config & state

`$TERVA_HOME/connectors/matrix/` (state dir, ours):

```
config.json       — homeserver_url, user_id, device_id, auto_join ("always" | "never", default "always"), speaker ("off" | "name_only" | "full", default "off"), max_attachment_mb (default 64), e2ee (setup's provisioning summary), and — since 0.13.0 — the matrix-sdk session bundle under "session" (0600)
secrets.key       — our own age identity, SDK-minted on the first sealed save (0600; on the host's read deny-list)
session.json      — pre-0.13 home of the session bundle; migrated into config.json and removed before the first hello
store/            — matrix-sdk sqlite state + crypto stores
initial-sync-done — the first-connect marker (client.rs)
data/             — the host-assigned data_dir (attachment staging; host sweeps it)
pairing.json      — HOST-owned; never read or write
```

Config is created by `setup`, read by `run`/`status`/`configured`. `status`
prints the **shape** — homeserver, MXID, device id, store-exists,
verified-state — and masks every secret.

Since 0.13.0 the token values inside `config.json` are **sealed in place**
(`enc:age:v2`, dual-recipient: our key and terva's) whenever the host has
at-rest encryption configured, and the hello declares those paths. The
session bundle lives inside the declared file rather than beside it
precisely because the declaration is what the host's per-read gate
trusts — see `docs/architecture.md` §"State on disk" for the full argument.

## 6. Crate architecture

```
src/
├── main.rs        — thin binary: verb dispatch on argv[last]; tokio runtime
│                    only for `run`
├── lib.rs         — the crate the binary and tests build on (seeded)
├── proto.rs       — connproto frame types + serde (seeded; golden-tested)
├── wire.rs        — LF-JSON framing over stdio: reader thread → mpsc,
│                    single writer task, 4 MiB skip-and-warn (seeded)
├── serve.rs       — the connsdk.Serve analog: handshake, negotiated-version
│                    state, command dispatch, result correlation, shutdown
├── config.rs      — state-dir layout, config/session load-store
├── setup.rs       — interactive verbs (setup/status/reset/configured)
└── matrix/
    ├── client.rs  — login/restore, sync service, connected/connect_error
    ├── rooms.rs   — room→chat mapping, dm detection, thread-id codec
    ├── inbound.rs — timeline event → message/edited/deleted/reaction frames
    ├── outbound.rs— send/edit/react/delete/thread_start/typing handlers
    ├── media.rs   — download-decrypt to data_dir, upload, kind mapping
    └── asks.rs    — reaction-widget lifecycle (seed reactions, answer
                     mapping, ask_close re-render)
```

- **Runtime**: tokio multi-thread. stdin is read on a dedicated blocking
  thread feeding an mpsc channel (async stdin is not worth the trouble);
  stdout writes go through one writer task — the only place that ever touches
  stdout.
- **Logging**: `tracing` → stderr, always. A `panic` hook that logs before
  dying keeps the host's log useful.
- **Deps** (added as each phase needs them): `serde`/`serde_json` (seeded),
  `matrix-sdk = "0.18"` (defaults: e2e-encryption, sqlite, qrcode; plus
  `markdown`), `tokio`, `tracing` + `tracing-subscriber`, `anyhow`.
  `Cargo.lock` committed; every build `--locked` (house convention).

## 7. Testing strategy

1. **Golden frame tests** (seeded, grow with each phase): port the literal
   JSON lines from `connproto_test.go:TestGoldenFrames` and assert byte-exact
   encoding (serde emits declaration-order keys; our structs mirror the Go
   field order, so byte equality is attainable) and tolerant decoding
   (unknown fields, unknown frame types).
2. **Wire smoke** (seeded): spawn the real binary (`CARGO_BIN_EXE_`), script
   `hello_ack → connect → shutdown` and assert the `hello → connect_error →
   clean-exit` conversation — the Rust analog of terva's `proxy_test.go`
   helper-process tests. Grows into: happy-path send/result, unknown-frame
   tolerance, stdin-EOF exit, oversized-frame skip.
3. **run.sh discipline** (seeded): the launcher has the exec bit (git tracks
   it; a plain file write drops it) and the manifest/Cargo versions are in
   lockstep — both plain unit tests, per the house checklist.
4. **Matrix layer**: `matrix_sdk::test_utils::mocks::MatrixMockServer`
   (wiremock) for room-mapping, inbound-translation, and media tests without
   a homeserver; a `just synapse` docker-compose target (in place —
   `testing/synapse/`, driven by `just e2e` → `tests/live_synapse.rs`) for
   live compliance and, later, E2EE/verification testing against a
   throwaway Synapse.
5. **Live conformance**: `terva bot link` this repo, `terva bot run
   --connector matrix` against a real room on the local homeserver; the
   dogfood loop that graduated the Discord connector.

## 8. Phases

Each phase lands with its tests, its feature-string declarations (never
declare ahead of implementation), and a CHANGELOG entry.

- **P0 — seed** (this commit): plan, manifest, launcher, justfile, proto.rs
  (full v2 frame set), wire.rs, verb dispatch, handshake-then-`connect_error`
  skeleton, golden/wire/version tests. Runs under a real terva today and
  reports itself unconfigured honestly.
- **P1 — wire completeness** (landed, 0.2.0): serve.rs state machine
  hardening (result for every id'd command even when unhandled, warn
  plumbing, oversized-frame guard, panic hook), full golden-corpus port,
  EOF/shutdown/timeout smokes. Exit criteria: the conversation transcript
  from `loop_e2e_test.go` replayed byte-for-byte against our binary — met at
  protocol 2 in `tests/serve_replay.rs` (the original test speaks protocol 1
  through the extension tunnel; the connproto conversation inside it is what
  we replay).
- **P2 — login + DM text round trip** (landed, 0.3.0): matrix-sdk client,
  `setup`/`status`/`reset`/`configured` for password login, session restore,
  sync loop with backlog discard, DM mapping, inbound text `message`,
  outbound `send` (+ markdown), `typing`, `result` correlation. Declares:
  `message_ids`, `chat_kinds` (informative), `max_text_len`,
  `typing_refresh_ms`. Exit: paired DM conversation with a real agent
  through a real homeserver — pending live dogfood; the mock-homeserver
  suite covers everything up to the wire.
- **P3 — E2EE** (landed, 0.4.0): sqlite crypto store, encrypted-room
  send/receive, recovery-key create/restore + SAS verification in `setup`,
  UTD warns (once per burst, with automatic backup key download), `reset` =
  server-side logout + wipe. Encrypted attachment plumbing rides the media
  module P5 fills out; the SDK's `send_attachment`/media download decrypt
  transparently, so nothing E2EE-specific remains for P5.
- **P4 — groups** (landed, 0.5.0): `chat_membership` for the bot's own
  admission (join attributed to the inviter, leave/kick/ban to the doer,
  idempotent via an announced-set seeded at connect), `entities` with
  `bot_mention` from `m.mentions` + legacy pills + reply-to-our-events
  (span by candidate scan, code-point offsets, 0/0 when unlocatable),
  `mention` for other users' pills. Declares: `entities`,
  `chat_membership`. Admission-flow dogfood with the host gate rides the
  next DOGFOOD pass.
- **P5 — message events + attachments** (landed, 0.6.0): edits/deletes/
  reactions in and out (edits under the original id; reaction removal
  reactor-attributed via a seen-reactions map, our reaction ids cached
  with a `/relations` rebuild fallback), attachment ingest all kinds
  (voice via MSC3245, stickers via their own event type, v1.10 caption
  semantics) with the ceiling enforced on declared AND actual size,
  `send_image`/`send_file` upload with mime-routed msgtypes. Declares:
  `edits_in/out`, `deletes_in/out`, `reactions_in/out`,
  `attachment_kinds`, `sends_images`, `sends_files`,
  `min_edit_interval_ms: 1000`.
- **P6 — asks** (landed, 0.7.0): reaction-widget lifecycle (legend +
  seeded reactions, ①②③ hint fallbacks), `attested` answers off signed
  events with the federation caveat documented, exact-MXID `restrict_to`
  filtering (imposter taps best-effort redacted), `ask_close` seed
  withdrawal + outcome render, `expires_ms` self-withdrawal. Declares:
  `asks`. Exit still to dogfood: `--approval ask` bot mode driven
  entirely from a Matrix room.
- **P7 — threads** (landed, 0.8.0): the `<room_id>;thread=<root>` codec on
  every outbound command, `thread_start` (anchored + anchorless, the name
  as starter), inbound routing with root-snippet titles and
  real-reply-only `reply_to`, `is_falling_back` fallback fed by a
  per-thread latest-event cache. Declares: `threads_out`.
- **P8 — polish + release** (landed, 0.9.0): MSC4144 speaker behind the
  `speaker` config key (per-message profiles via
  `com.beeper.per_message_profile`, avatar upload cache at `full`),
  429/Retry-After posture documented at the request-config site (the SDK
  honors it on every REST path), `just dist` prebuilt archives with the
  launcher preferring the bundled binary, README setup walkthrough.
  MSC3381 polls remain a future enhancement; the graduation dogfood is
  operator-side.

## 9. Risks & open questions

- **E2EE verification UX on a headless tty** — recovery-key entry is fine;
  SAS emoji comparison over a terminal is awkward but shippable. Worst case:
  document "verify from another client".
- **Thread-chat derived ids** are a convention only this connector knows;
  if terva ever grows first-class thread semantics the codec may need a
  migration.
- **Reaction-widget asks in E2EE rooms**: ask option `key`/`hint` ride
  reaction keys, which are **cleartext even in encrypted rooms** (relations
  aren't encrypted) — the contract already warns about this; never put
  sensitive content in option keys/hints/labels rendered as reactions.
- **Backlog boundary correctness**: resolved in 0.10.0 — history discards
  only on the first-ever connect; warm restarts resume from the store's
  token and recover downtime (the live suite pins both). The crash window
  between the store persisting a token and our handlers delivering that
  batch is the residual at-most-once edge; it is one sync batch wide.
- **Cold owner addressing**: resolved in 0.11.0 — until the owner's first
  inbound DM of a host run, terva addresses owner-directed frames (the
  admission ask, tool approvals, the idle nudge) by the paired USER id,
  which on Matrix is not a chat id. `resolve_joined_room` now takes a chat
  id that parses as a user id to the `m.direct` DM room with that user;
  with no joined DM the `result.error` names the mis-addressing and points
  at the upstream bug. The residual is upstream, not ours: the host writes
  its ask-suppression flag before its unpaired guard, so an admission ask
  burned while unpaired stays burned for that process run and no
  re-announcement heals it (connproto-proposals §12). Our mitigation is
  dormant-safe if that fix lands and the user-id seeding goes away.
- **Rate limiting**: Synapse default message PUT limits are bursty; the
  min-edit-interval plus 429 retry handling must be verified under streamed
  edits before declaring `edits_out`.
- **matrix-sdk 0.18 API churn**: the SDK is pre-1.0 and reworks surfaces
  between minors; pin exact, upgrade deliberately, keep the Matrix layer
  behind our own traits so churn stays out of serve.rs.
- **Sticker/voice inbound kinds** vary by client MSC adoption; kind mapping
  should degrade to `document`/`audio` rather than drop.

## 10. References

- terva connector docs: `terva/docs/connectors.md` (frame reference, rules,
  feature vocabulary) · `terva/docs/plans/archive/chat-connectors.md` (Phase 6:
  Matrix, external-first) · `terva/docs/proposals/archive/connector-protocol-v2.md`
  (as-built v2 spec incl. the Matrix deep-dive) ·
  `terva/docs/decisions/0002-connector-event-sources-hold.md` (no event
  frames — do not invent one).
- Reference implementations: `terva/cmd/terva-telegram-connector/main.go`,
  `terva/cmd/terva-discord-connector/main.go` (connsdk consumers) ·
  `terva/packages/agent/chat/extconn/loop_e2e_test.go` (sh connector).
- House Rust conventions: `terva-sh/terva-ext-index` (launcher, justfile,
  `--locked`, version lockstep, stderr discipline) ·
  `terva-sh/docs/docs/extensions/conventions.md`.
- Matrix: [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)
  ([matrix-sdk 0.18.0](https://docs.rs/crate/matrix-sdk/latest), 2026-06) ·
  [spec](https://spec.matrix.org/latest/) · threads (v1.4+), `m.mentions`
  (v1.7+), captions (v1.10) ·
  [MSC3381 polls](https://github.com/matrix-org/matrix-spec-proposals/pull/3381) ·
  [MSC4144 per-message profiles](https://github.com/matrix-org/matrix-spec-proposals/pull/4144) ·
  MSC3245 voice messages.
