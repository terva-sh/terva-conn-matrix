# Changelog

## 0.14.3 — inherited thread admission (2026-10-01)

- Thread messages report their parent room and kind after the host and
  connector negotiate `chat_parents`. The host can apply parent admission
  policy while each thread keeps a separate conversation.
- Pin Rust SDK v0.4.0 instead of the temporary development revision.


Phases refer to [PLAN.md](PLAN.md) §8.

## 0.14.2 — a `verify` verb, because setup could lose the verdict (2026-09-08)

- **New `verify` verb: re-read the E2EE verdict on the session you already
  have.** `setup` writes `cfg.e2ee` only after its interactive tail
  returns, so a setup killed during recovery or the 120 s SAS wait leaves a
  working session and no verdict at all. Until now the only way to answer
  the question again was `setup`, which logs in unconditionally and costs a
  second registered device. `verify` restores the saved session against the
  existing store instead: no login, no new device, no recovery key. It
  prints the verdict, records it, and offers SAS only when the device is
  not already verified. terva never calls it; an operator runs it by hand.
- **It forces a key query rather than trusting a sync.** Signatures live on
  the server, so the stored copy of our own device goes stale the moment
  another client signs it. Refreshing by `sync_once` is not enough: a sync
  issues the key query only when the response puts us in
  `device_lists.changed`, and a signature uploaded while we were offline
  need not appear there on an initial sync. Measured on a live deployment,
  a sync-refreshed read said `unverified` and `request_user_identity`
  said `verified` against the same store seconds later. `verify` issues the
  query outright, which also keeps it off the connector's sync-resume
  position.
- **`status` no longer calls a missing verdict `unknown`.** An empty
  `e2ee` means no verdict was ever recorded, which is not the same claim as
  "the device is not verified"; it now reads `not recorded` and names the
  verb that fixes it. A new `e2ee_source` field records which verb wrote
  the snapshot, so `status` says `(as of setup)` or `(as of the last
  verify)` rather than always crediting setup. The field defaults to empty
  and older configs keep reading as setup's, which is what they were.
- Known gap, filed separately: `setup` still decides whether to offer SAS
  from `verification_summary`, which reads an observable that nothing in
  the setup path can move, so it reports `unknown` even when recovery just
  cross-signed the device. `verify` is the way out of that today; setup
  itself is unchanged here.

## 0.14.1 — clean on clippy 1.98 (2026-09-01)

No behavior change. The public mirror's first CI run failed on Rust
1.98's new `result_large_err` lint: `build_client` returned matrix-sdk's
~160-byte `ClientBuildError` by value, taxing every `Ok(Client)`. The
error is boxed now — it is built once per connect, so the box costs
nothing that matters — and the gate here runs the same clippy the
runners do. The public `v0.14.0` tag stays: its tree builds and its
tests pass; only the lint gate was red.

## 0.14.0 — the typing indicator clears with the reply (2026-08-31)

- **Declares `typing_stop` and clears the indicator on the host's stop
  frame.** connproto proposals §6 shipped upstream (terva #863): a
  `typing` with `active:false` follows each reply, sent only to a
  connector that declared the feature. We PUT `typing: false` on it, so
  "…is typing" ends when the answer lands instead of running out the
  30 s server-side timeout beside it — the README's first known
  limitation, gone on a host that speaks it and unchanged on one that
  does not (an older host never sends the frame; an older connector
  would never be sent it). The pulse is exactly as before.
- Pinned by the hermetic mock test (the pulse PUTs `typing:true` with
  our 30 s timeout, the stop PUTs `typing:false`), the wire smoke (a
  stop owes no result, like the pulse), and the live DM scenario (the
  human's sync sees the bot vanish from the typing set on the stop).
- SDK pins bumped to **terva-sdk-rust v0.3.4** (`TypingFromHost.active`,
  the corpus with the `typing stop` case).

## 0.13.3 — housekeeping: SDK v0.3.3, the graduation walkthrough (2026-08-31)

No behavior change.

- **SDK pins bumped to terva-sdk-rust v0.3.3.** 0.3.1–0.3.3 are
  test-only on the SDK side — `terva-connproto` now drives its
  conformance suite from terva's published `golden.jsonl` (the fixture
  proposals §10 asked for), refreshed for the `dir` envelope — so
  nothing this binary links changed. Re-pinned so the ecosystem sits on
  one SDK version again, which is the claim 0.13.2 made and the three
  releases since had quietly falsified.
- **`testing/DOGFOOD.md` is the graduation walkthrough now**, not the
  P2 exit check it had stayed since 0.3.0. PLAN.md claimed it covered
  groups and asks; it covered groups as one optional row and asks not
  at all, and its closing note still said a message missed while down
  "is lost by design" — the pre-0.10.0 rule, directly under a row
  asserting the opposite. The rewrite walks the whole shipped surface
  in the order it was built (DM, recovery, E2EE, groups and admission,
  message events and attachments, asks, threads, speaker, hygiene — 39
  rows), runs the host with `--approval ask` so the widget actually
  fires, and is honest about what the host cannot drive yet: outbound
  `thread_start`, `edit`, `react`, and `delete` have no caller in the
  host `Loop` today, so those stay proven by the live suite alone. The
  same checklist is the real-homeserver run; only the two setup
  sections differ.
- PLAN.md's status paragraph says the same.

## 0.13.2 — SDK pins at v0.3.0 (2026-08-04)

Pin bump only; no behavior change here. The SDK's 0.3.0 is breaking for
extensions that carry a connector (its protocol-5 carrier moved behind
an off-by-default feature) and touches nothing a standalone connector
uses — this connector depends on connproto/connsdk/wire/env, none of
which changed. Re-pinned so the ecosystem sits on one SDK version, and
verified the way that claim deserves: the full suite plus all eight live
Synapse scenarios.

## 0.13.1 — config persistence through the SDK's typed pair (2026-08-04)

- **`load_config`/`save_config` go through `SealedState::load_as`/
  `save_as`** (new in SDK v0.2.1) instead of hand-rolled serde around
  the byte API. The on-disk file is unchanged — same shape, same
  sealing, same permissions — but the load error no longer carries
  serde's message, which quotes the value it choked on. By the time
  `load_config` parses, that value is an OPENED access token, so a
  future config schema change can no longer put the live token into
  whatever we log.
- SDK pins bumped to **terva-sdk-rust v0.2.1**.
- Dropped `default_config()`, dead since the 0.13.0 load path stopped
  using it, and took the rustfmt 1.9 reflow that came with the
  toolchain update.

## 0.13.0 — the access token is sealed at rest, and declared (2026-08-04)

Adopts terva's secrets-at-rest phase 3 (its proposal §8; SDK v0.2.0):

- **The matrix-sdk session bundle moved INTO config.json** (under
  `"session"`), and its token values are **sealed in place** —
  `enc:age:v2:` strings, dual-recipient (our own key at
  `connectors/matrix/secrets.key`, minted on the first sealed save, plus
  terva's public recipient from its config.json), each value BOUND to
  its path so a moved ciphertext refuses to open. Everything else stays
  plaintext and inspectable. On a host that never ran
  `terva secret init` the file is written plaintext exactly as before,
  converting on the first save after encryption turns on.
- **The hello declares our recipient and secret paths**
  (`/session/access_token`, `/session/refresh_token`) via
  `terva_connsdk::SealedState` — what lets terva re-seal our file during
  a key rotation without ever holding our key, and what earns
  `connectors/matrix/` its agent-readability under the host's new
  per-read gate (terva `246f23d3`: unregistered directories are dark).
  The declaration is silent until a key actually exists.
- **A pre-0.13 session.json is migrated into config.json and removed**
  before the first hello — the declared paths must cover every
  credential in the directory, or the declaration lies by omission.
  `setup` writes the merged shape directly; `reset` semantics unchanged.
- SDK pins bumped to **terva-sdk-rust v0.2.0** (connproto hello
  `secrets` field, connsdk `SealedState`, extproto 6, the extsdk secret
  broker). Cross-language sealing interop is proven in the SDK against
  the real Go implementation, both directions.

## 0.12.1 — the SDK dependencies pin a release tag (2026-08-03)

- **The terva-sdk-rust crates are now git-pinned at its `v0.1.0` tag**
  (internal origin) instead of path deps — the SDK's P8 release model:
  pin for release, path for dev. A source build no longer needs the
  sibling checkout; any host that can fetch the internal remote builds
  from a bare clone (`.cargo/config.toml` turns on
  `net.git-fetch-with-cli` for the ssh fetch). Co-development uses the
  commented `[patch]` block at the bottom of `Cargo.toml`. No behavior
  change — the pinned tag is the exact SDK state 0.12.0 was built
  against, plus the SDK's own P6-P8 additions (none of which this
  binary exercises yet).

## 0.12.0 — the protocol layer moves to terva-sdk-rust (2026-08-03)

- **`proto`/`serve`/`wire` and the generic env/persistence half of
  `config.rs` now come from the shared
  [terva-sdk-rust](https://github.com/terva-sh/terva-sdk-rust) crates** (`terva-connproto`,
  `terva-connsdk`, `terva-wire`, `terva-env`), re-exported under their
  historical module names so the rest of the tree — and the binary's
  behavior — is unchanged. The golden-corpus and serve-replay
  conformance suites moved with the SDK; this repo keeps the
  binary-level smokes (`wire_smoke`, `verbs`, `conventions`) and the
  whole Matrix layer. Path dependencies for co-development; a git-rev
  pin comes when the SDK API settles (its P8). Building from source now
  expects the `terva-sdk-rust` checkout as a sibling; `just dist`
  archives are unaffected (the bundled binary is preferred by
  `run.sh`).

## 0.11.0 — user-id chat ids resolve to the owner's DM (2026-08-03)

- **A `chat_id` that is a user id now resolves to the `m.direct` DM
  room with that user.** Until the owner's first inbound DM of a host
  run, terva addresses owner-directed frames — the admission ask, tool
  approvals, the idle nudge — by the paired *user* id, which on Matrix
  is not a chat id; those frames used to fail (and the host swallows
  admission-ask failures fail-closed, so the owner was never prompted
  — the likely mechanism behind the dogfood's missing-admission-DM
  symptom, filed upstream as proposals §12). Resolution follows the
  addressing rule drafted in doc-proposals item 7: resolve to the
  existing DM, or fail with a `result.error` that names the
  mis-addressing and points at §12. The live DM scenario now proves a
  user-id-addressed send lands in the DM; the no-DM diagnostic is
  pinned hermetically.
- README gains the `docs/` index (the four-document connector↔host
  feedback conversation) and a **Known limitations** section: the
  typing tail, cold owner addressing without a DM, pre-restart thread
  redaction scope, and the recovery window/at-least-once boundary.

## 0.10.1 — thread-resident message events carry the delivered chat id (2026-08-03)

- **Edits, deletes, and reactions touching a message that lives in a
  thread chat now carry the derived `<room>;thread=<root>` chat id the
  message was delivered under — not the room's.** Found by the terva
  host's review of our proposals doc
  ([docs/connproto-proposals-host-review.md](docs/connproto-proposals-host-review.md)):
  the host correlates message events on the compound key
  `(chat_id, id)`, so the room-scoped frames 0.10.0 emitted missed the
  thread's queue entirely — an edited prompt kept its stale text, the
  edit note landed in the wrong per-chat agent, and (the sharp one) a
  deleted thread message still became an agent turn. A session cache
  records every delivered and sent event's scope; targets unknown to
  the cache (edited after a connector restart) resolve by fetching the
  original and reading its thread relation. Redaction targets are
  cache-only (a redacted event has its relations stripped) and degrade
  to room-scoped. The live thread scenario now drives all three event
  kinds through a thread and pins their chat ids.

## 0.10.0 — dogfood fixes: downtime recovery + diagnosability (2026-08-03)

- **Messages sent while the connector is down are now DELIVERED on
  reconnect.** The P2 "never deliver backlog" rule discarded everything
  at every connect — the dogfood found that a crash-restart cycle
  silently eating messages reads as a broken bridge, not discipline
  (and it also starves the host's first-message admission fallback for
  chats whose `chat_membership` frame it missed). New semantics: only
  the FIRST-ever connect discards history (a freshly set-up bot still
  never replays pre-setup rooms into agent turns; a marker file records
  that it ran, wiped by `reset`/re-`setup`). Every later connect
  resumes the live sync from the token the sqlite store persisted, so
  downtime is recovered — bounded by the server's limited-timeline
  window, so a long outage delivers the most recent messages per room,
  not the whole gap. Warm connects validate the token with `whoami`
  (no sync in the connect path at all any more). The live suite's
  restart assertions are flipped accordingly, for the plain AND the
  encrypted DM (missed ciphertext decrypts on arrival — its room key
  rides the queued to-device traffic).
- **Encrypted groups verified end-to-end**: a new live scenario proves
  the admission `chat_membership` fires for an encrypted room exactly
  as for a plain one (membership is state, never encrypted) and that a
  mention inside the encrypted group arrives decrypted with its
  `bot_mention` entity — the connector side of the reported
  encrypted-room silence was already sound; the missing piece was the
  recovery fix above.
- **Diagnosability pass** — `connector-matrix.log` now tells the story
  on its own: a startup banner (version, protocol, state dir), the
  connect path taken (first-connect discard vs warm resume), the sync
  loop's starting point, one INFO line per frame delivered to the host
  (event/chat ids, kinds, sizes — never message content, never tokens),
  every filtered path saying why at debug (own echo, non-text edits),
  every failed command logged at the single `Responder::err` choke
  point, and ask lifecycle events. `RUST_LOG` overrides the filter
  (e.g. `RUST_LOG=debug` adds a full type-only wire trace;
  `matrix_sdk_crypto=trace` for E2EE forensics).
- Dogfood tooling: `just synapse` now also brings up an Element Web
  (http://127.0.0.1:18009) preconfigured against the throwaway Synapse —
  register and go, no homeserver editing. Human side only; the bot and
  the live suite keep talking to Synapse directly.

## 0.9.0 — P8: polish + release (2026-08-02)

- **MSC4144 per-message speaker profiles**, config-gated. A new `speaker`
  key in config.json (`off` default / `name_only` / `full`) declares
  `speaker:name_only` or `speaker:full`; with the flag off nothing is
  declared and the host's `**Name:**` prefix fallback keeps the cast
  working in every client. Enabled, `send.speaker` renders as
  `com.beeper.per_message_profile` (`id` = the stable speaker key,
  `displayname`, and at `full` an `avatar_url` uploaded once per
  (key, path) through a bounded cache) via `send_raw`, which rides the
  ordinary send machinery — E2EE included. Cast rendering never fails a
  send: avatar trouble degrades to name-only. `status` shows the knob.
- **Rate limits**: documented at the request-config site — the SDK
  classifies 429/M_LIMIT_EXCEEDED as transient and honors Retry-After
  when scheduling its retries on every REST path; with the declared
  `min_edit_interval_ms` that is the 429 posture, and anything still
  limited after retries surfaces as an honest error result.
- **Release packaging**: `just dist` builds
  `dist/terva-conn-matrix-<version>-<target>.tar.gz` (prebuilt binary,
  manifest, launcher, docs); `run.sh` now prefers a bundled binary
  beside it, so the unpacked directory links and runs on machines with
  no Rust toolchain.
- **README**: a real-homeserver setup walkthrough (dedicated account,
  E2EE recovery/verification prompts, config knobs, reset), and the
  dist story.
- The live suite grows a speaker scenario: the config flag declares
  `speaker:full` in the hello, a cast send lands with the profile and a
  real `mxc://` avatar on the raw event, and a speaker-less send stays
  an ordinary bot message.

## 0.8.0 — P7: threads (2026-08-02)

Declares `threads_out`. Matrix threads are relation trees inside a room,
not first-class chats, so a thread chat rides connproto's opaque chat ids
as **`<room_id>;thread=<root_event_id>`** — a convention private to this
connector, documented in the README.

- **`thread_start`** sends the thread's name as its starter: anchored, it
  threads off `from_message_id`; anchorless, the starter itself becomes
  the root and later sends thread off it. The result's `chat_id` is the
  derived thread chat; sends, asks, attachments, and typing all accept
  it (typing is room-scoped by nature; attachments ride the thread via
  the SDK's reply config).
- **Outbound threading** keeps the spec's `is_falling_back` reply
  fallback pointed at the latest in-thread event we know (per-thread
  bounded cache, root when cold); an explicit `reply_to` becomes a real
  in-thread reply instead.
- **Inbound routing**: events carrying an `m.thread` relation land in the
  derived thread chat — `chat_kind: "thread"`, `chat_title` a best-effort
  snippet of the root event (bounded cache, one 3 s fetch, failures
  cached empty). A thread fallback's `in_reply_to` is rendering
  compatibility, NOT intent — `reply_to` now flows only for real replies
  (this also stops pre-P7 behavior treating every thread message as a
  reply to whatever it fell back to, which inflated reply-to-bot
  mention signals).
- The live suite grows a thread scenario: anchored `thread_start` with
  the relation proven on the wire, a send into the derived chat, the
  human's thread traffic routing back with the root-snippet title,
  a real in-thread reply keeping `reply_to`, and typing into the
  thread chat reaching the room.

## 0.7.0 — P6: reaction-widget asks (2026-08-02)

Declares `asks`. Matrix has no buttons, so an ask renders as its text
plus an emoji legend (`- 👍 Approve`, hints when given, circled-digit
①②③ fallbacks otherwise, duplicates deduped), and the bot seeds its own
message with one reaction per option — tapping a seed answers.

- **Answers are `attested`** — a Matrix reaction is a signed event whose
  sender the origin homeserver authenticated, unlike the reactions of the
  platforms the generic contract had in mind. The honest caveat lives in
  the module docs: a malicious *federated* homeserver can forge events
  for its own users only, and an MXID embeds its server, so the exact-MXID
  `restrict_to` match (which the host re-checks anyway) keeps grants
  sound unless the owner's own homeserver is hostile.
- **`restrict_to` filters service-side**: an unauthorized tap is never
  answered and is best-effort redacted so the widget stays readable
  (needs power over others' events; failure is silently tolerated).
  Emoji keys are matched with VARIATION SELECTOR-16 stripped, so a
  picker-typed 👍️ matches the seeded 👍. An unrelated emoji on the ask
  message falls through as a plain `reaction` for the host to note, and
  an un-tapped answer is swallowed as widget traffic (never a
  `message_deleted`).
- **`ask_close`** withdraws the seed reactions and renders the outcome
  into the question message (`text` + bold outcome — the audit trail
  lives in the channel). Ask state is session-local by design: closing
  an ask from before a restart errs honestly and the host's lifecycle
  copes.
- **`expires_ms`** arms a watcher (in the service's task set, like every
  Shared-holding task): expiry withdraws the widget and stops
  translating — late taps are plain reactions — but keeps the state so
  a late `ask_close` still renders its outcome.
- The live suite grows an ask-lifecycle scenario: legend + both seeds
  visible on the server via `/relations`, an attested answer from the
  restricted-to human, close withdrawing the seeds and rendering the
  outcome, and a 2 s expiry ask whose widget self-withdraws and whose
  late tap arrives as a plain reaction, never an answer.
- **Fixed a warm-reconnect stall the suite exposed**: the connect-time
  backlog-discard sync used the SDK's default 30 s long-poll, and the
  warm sqlite store supplies a remembered sync token — so on every
  restart of a QUIET account the homeserver held that request the full
  30 s before `connected` could go out (a fresh store returns
  immediately, which is why first connects and busy-room restarts never
  showed it). The discard sync's only job is recording `next_batch`; it
  now asks for `timeout=0` and returns at once in both cases.

## 0.6.0 — P5: message events + attachments (2026-08-02)

Declares `edits_in/out`, `deletes_in/out`, `reactions_in/out`,
`attachment_kinds`, `sends_images`, `sends_files`, and
`min_edit_interval_ms: 1000` (edits are the rate-limit-prone path).

- **Message events in** (`inbound.rs`) — an `m.replace` becomes
  `message_edited` under the ORIGINAL event id (latest-wins is inherent in
  the relation; entities recomputed from the new content; the bot's own
  streaming edits stay silent). A redaction becomes `message_deleted` —
  unless it redacts a reaction the session delivered, in which case it is
  `reaction { removed: true }` attributed to the REACTOR, not to whoever
  redacted it (a bounded map keyed by reaction event id carries that
  memory; its miss degrades to a delete of an id the host never met, which
  the host ignores). Inbound reactions apply the same echo hygiene as
  messages: the bot's own toggles never come back.
- **Message events out** (`outbound.rs`) — `edit` sends a markdown-rendered
  `m.replace` (with the `* text` fallback for clients that never learned
  edits), `delete` redacts, `react` sends an `m.annotation` and remembers
  (message, key) → our reaction event id so `remove` is a single redact;
  entries lost to a restart or eviction rebuild lazily from `/relations`
  filtered to our own annotations.
- **Attachments** (`media.rs`) — inbound `m.image`/`m.audio`/`m.video`/
  `m.file` (plus `m.sticker`, its own event type) download — decrypting in
  E2EE rooms, the SDK does both in one call — into the host-assigned
  `data_dir` under collision-free sanitized names, typed
  image/audio/voice/video/document/sticker (voice via the MSC3245 marker),
  with Matrix v1.10 caption semantics (`filename` set + differing `body` =
  caption) and size/duration metadata. The `max_attachment_mb` ceiling is
  enforced BEFORE the transfer on the declared size and again on the
  actual bytes (info sizes are advisory; a lying event must not land 2 GB
  in `data_dir`); over-ceiling and failed downloads drop the message with
  a `warn`. Outbound `send_image`/`send_file` read the host's path, guess
  the mime from the extension, and upload via `send_attachment` (encrypts
  in E2EE rooms; the mime routes the msgtype, so images render as
  `m.image`), captions included.
- The live suite grows a message-events scenario: edit/reaction(+removal)/
  delete round trips in both directions (redacted server content proven
  empty), file-with-caption and image sends verified on the human's
  client, an inbound image ingested byte-exact into `data_dir` with kind/
  caption/mime intact, and a cold-cache `react remove` after a restart
  proving the `/relations` rebuild.

## 0.5.0 — P4: groups (2026-08-02)

Declares `entities` and `chat_membership` — the two signals terva's
group-admission gate runs on (mention-gated, owner-approved; the
connector stays deliberately dumb about trust).

- **`chat_membership` frames** (`inbound.rs`) — the bot's OWN admission
  changing, never other members' churn. A join is announced once, `added`,
  attributed to the inviter (remembered from the live invite event, or read
  via `invite_details` for invites that predate the session — that read
  must happen before joining, while the room is still in the invited
  state). A leave/kick/ban is `removed`, attributed to the event's sender.
  An announced-set seeded with the joined rooms at connect keeps
  pre-session memberships silent under state re-delivery and makes the
  frames idempotent; a rejected invite (never joined) emits nothing.
- **`entities` markup** (`entities.rs`) — inbound messages carry
  `bot_mention` when the intent is real: our MXID in `m.mentions`
  (Matrix v1.7), a legacy `matrix.to`/`matrix:` user pill in the HTML
  `formatted_body` (hand-rolled anchor scan — including the rich-reply
  fallback's sender pill), or a reply to one of our own messages (a
  bounded cache of our outbound event ids). The span is located in the
  delivered text by candidate scan — pill anchor text, then the explicit
  id forms, then the profile display name (last, because servers default
  it to the localpart, which would otherwise mis-anchor inside a typed
  MXID — caught live by the group scenario). Offsets are Unicode code
  points; 0/0 means "mentioned, not locatable", which the host gate
  accepts. Other users' pills become `mention` entities with `user_id`
  when locatable. The name merely appearing in text is NOT a mention —
  plain-text scanning is the host's fallback, deliberately not ours.
- The live suite grows a group scenario: named-room invite → auto-join →
  `added` attributed to the inviter; an unaddressed message carries no
  entities (the gate must see silence); an `m.mentions` message carries a
  located `bot_mention`; a reply to the bot's message reads as a span-less
  mention; a kick emits `removed` attributed to the kicker.

## 0.4.0 — P3: E2EE + the live compliance harness (2026-08-02)

- **E2EE** (`matrix/e2ee.rs`) — the Matrix-specific hard part. Encrypted
  send/receive is SDK-transparent (rooms with `m.room.encryption` encrypt
  outbound events and decrypt inbound ones before our handlers see them);
  what landed around it:
  - `setup` gains the verification story: create a new recovery key
    (cross-signing bootstrap with the login password answering the UIAA
    challenge, key backup enabled, the key printed once), restore from an
    existing recovery key, or skip — plus an optional interactive emoji
    (SAS) verification driven on the tty against another logged-in client.
    All best-effort: an E2EE hiccup never costs the saved session. The
    summary is snapshotted into config and shown by `status`.
  - Unable-to-decrypt events warn the operator once per burst (60 s
    limiter, unit-tested), log per event, and the client is configured
    with `BackupDownloadStrategy::AfterDecryptionFailure` so backed-up
    keys are fetched automatically. Auto cross-signing/backup enablement
    stays OFF — those flows mint a replacement identity and would be
    destructive on an account that already has one; `setup` drives them
    explicitly by user choice.
  - The live suite grows an encrypted-DM scenario: bootstrap + recovery
    via headless `setup`, encrypted round trip both ways with the server's
    raw events proven to be `m.room.encrypted` (fetched outside the Olm
    machine), and crypto-store persistence across a restart (fresh
    process keeps decrypting; missed ciphertext is still never replayed).

- **Live compliance harness** (`testing/synapse/`, `tests/live_synapse.rs`,
  `just e2e`) — a throwaway dockerized Synapse (`server_name: localhost`,
  open registration, no federation, state gitignored) plus a gated
  integration suite that drives the real binary end-to-end with a second
  matrix-sdk client playing the human: headless `setup` → `configured`,
  connect/handshake, invite auto-join with the `is_direct` DM signal, text
  round trip (markdown rendering verified on the receiving client, rich
  replies both ways with fallback stripping), typing indicators, and the
  backlog-discard restart rule (a message sent while the connector is down
  must never be replayed; fresh traffic must flow). Hermetic `just test` /
  `just ci` are unchanged — the suite only runs under `just e2e`.
- `setup` reads the password as a plain stdin line when stdin is not a tty,
  so headless provisioning can pipe all three answers.
- **Two shutdown bugs the live suite caught on its first run** (the
  wiremock tests structurally could not see either):
  1. The Matrix event handlers captured `Arc<Shared>` while living inside
     the `Client` that `Shared` owns — a reference cycle that kept the
     session's `FrameSink` alive, so the writer channel never closed and
     the connector hung forever on `shutdown` (production would have
     masked it behind the host's SIGTERM escalation). Handlers now hold
     `Weak`, and the writer takes an explicit `Close` sentinel so prompt
     exit never depends on reference counts.
  2. The matrix-sdk sqlite pool aborts (SIGABRT) if destroyed outside a
     tokio runtime context. `MatrixService` teardown now reaps every
     background task and drops the client inside its runtime (`JoinSet` +
     explicit `Drop`), before the runtime itself goes.
  The live suite's `shutdown` helper now enforces the prompt-exit contract
  with a 10 s bound instead of waiting forever.

## 0.3.0 — P2: login + DM text round trip (2026-08-02)

The Matrix client lands on matrix-rust-sdk 0.18 (pinned exact — the SDK is
pre-1.0). Declares `max_text_len: 24000`, `typing_refresh_ms: 20000`, and
the informative `message_ids`/`chat_kinds` features. Exit criterion (a
paired DM conversation with a real agent through a real homeserver) awaits
the live dogfood; everything up to the wire is tested against
`MatrixMockServer`.

- **Lifecycle verbs** (`setup.rs`, `config.rs`) — `setup` does interactive
  password login on the tty (device name `terva`, password input hidden)
  and persists the SDK session bundle (0600, atomic writes) in the state
  dir `$TERVA_HOME/connectors/matrix/`; `status` prints the masked config
  block; `configured` is a pure local-file predicate; `reset` logs the
  device out server-side (best-effort, bounded) then wipes `config.json`,
  `session.json`, and `store/` — never the host-owned `pairing.json` or
  `data/`. Home resolution mirrors terva's envcompat (`TERVA_HOME`,
  deprecated `ZOT_HOME`, OS-default dirs with the zot fallback).
- **The Matrix service** (`matrix/`) — session restore into the sqlite
  state+crypto store, a backlog-discarding initial sync (a redeployed bot
  never replays history into agent turns), then a live sync loop with
  backoff, one `warn` per failure burst, and a fatal exit on
  `M_UNKNOWN_TOKEN` so the host's restart budget applies. Inbound text
  becomes `message` frames (own-echo filtered, edits deferred to P5, reply
  fallback quotes stripped, `reply_to` mapped); DMs are detected via
  `m.direct` plus the invite's `is_direct` flag (persisted back with
  `set_is_direct`); invites auto-join with retry, gated by the `auto_join`
  config. Outbound `send` renders markdown with rich-reply relations and
  returns the event id; `typing` PUTs a 30 s server timeout so the host's
  20 s refresh keeps it alive. Unconfigured `run` answers `connect_error`
  and keeps serving (never exits).
- **serve.rs for async services** — commands now complete through a
  `Responder` guard (exactly one `result`, even if a handler task panics or
  is dropped), `connect` distinguishes permanent failures (`connect_error`)
  from fatal ones (non-zero exit → restart budget), and `FrameSink::fatal`
  lets the sync loop end the session promptly. Reader/writer threads carry
  death guards so a panicked pump can never strand the serve loop.
- **Tests** — 56 total: mock-homeserver tests for inbound translation, echo
  hygiene, reply handling, DM mapping, markdown send, reply relations, and
  error paths; process-level verb smokes (token masking, reset wiping only
  our files, `configured` exit codes); all P0/P1 wire suites unchanged.

## 0.2.0 — P1: wire completeness (2026-08-02)

The connproto v2 wire layer is complete and hardened; nothing Matrix yet.
No features declared (capabilities grow with the phases that implement them).

- **serve.rs** — the connsdk.Serve analog: dedicated stdin reader thread and
  a single writer thread feeding stdout (deliveries/results/warns from any
  thread serialize cleanly), strict handshake (first frame must be a
  `hello_ack` inside our declared protocol range), connect state machine
  (failed connect stays retryable; a repeat connect re-answers from cache
  without restarting delivery), and a fatal-transport path that exits
  non-zero promptly so the host's restart budget applies. stdin EOF before
  or after the ack remains a clean exit.
- **Result correlation hardening** — every id-carrying command gets exactly
  one `result`, now including commands whose body fails to decode and
  unknown (future) command types: the `{type, id}` envelope is recovered
  separately so the host never waits out its 30 s timeout on us. (The Go
  connsdk silently drops these.)
- **Null tolerance** — Go marshals nil slices as `null`; all Vec frame
  fields (`options`, `restrict_to`, `entities`, `attachments`, `features`)
  now decode `null` as empty instead of rejecting the frame.
- **Oversize guards, both directions** — inbound over-cap lines are drained
  in constant memory (a hostile multi-GiB line no longer buffers whole) with
  Go lineframe parity on the edges (a line of exactly 4 MiB passes; a CR
  counts toward the cap). Outbound over-cap frames degrade loudly instead of
  being silently skipped by the host: a `result` shrinks to an error result,
  anything else becomes a `warn`.
- **Panic hook** — panics land in the host-captured stderr log with a
  backtrace before the process dies.
- **Tests** — full golden-frame corpus ported from terva's
  `connproto_test.go` (byte-exact encode for connector frames, decode
  assertions for host frames), the `loop_e2e_test.go` conversation replayed
  byte-for-byte at protocol 2 (the P1 exit criterion), and process-level
  smokes for the handshake deadline, fatal handshakes, id recovery, null
  tolerance, and oversize recovery. 42 tests.

## 0.1.0 — P0: seed (2026-08-02)

Plan, manifest, launcher, justfile, connproto v2 frame types (`proto.rs`),
LF-JSON framing (`wire.rs`), verb dispatch, and a handshake-then-
`connect_error` skeleton that runs under a real terva and reports itself
unconfigured honestly.
