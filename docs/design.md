# Design

The ideas behind terva-conn-matrix, and the components they produce.
This document stays at the level of concepts and decisions; the
companion [architecture.md](architecture.md) maps them onto the actual
code. The full protocol contract and the phase-by-phase build record
live in [PLAN.md](../PLAN.md).

## The problem

terva's bot mode bridges chat services to an agent through small,
isolated connector processes. This repo is the Matrix connector: a
standalone Rust binary that owns the Matrix wire (auth, sync, E2EE,
rendering) and speaks terva's connector protocol — **connproto v2**,
one JSON object per LF line over stdio — to a host that owns all
policy. The two sides meet at a deliberately thin waist: normalized
`message` frames one way, commands with correlated `result`s the other.

## Governing ideas

### Transport, not policy

The connector never decides who may talk to the agent. Pairing, group
admission, mention gating, approvals, queueing — all host-side. We
translate Matrix events into frames faithfully (including the signals
the host's gates feed on: `bot_mention` entities, `chat_membership`
frames) and render what the host asks for. This is the trust boundary
of the whole system, and it is why the connector can stay "deliberately
dumb": a compromised or buggy connector can garble a conversation, but
it cannot grant anyone reach.

### The wire is sacred

stdout is the protocol; a single stray byte corrupts it. Three rules
fall out:

- **One writer.** Every frame from every thread funnels through one
  writer thread. Nothing else may touch stdout, ever; all diagnostics
  go to stderr, which the host captures to a log file.
- **First-frame discipline.** `hello` must be on the wire within 3
  seconds of spawn, so it is emitted before *any* Matrix work — the
  connector never blocks startup on the network.
- **Declared, never assumed.** Optional constructs are emitted only
  when declared in our `hello` (and features never run ahead of their
  implementation); the host's `hello_ack` consume list is respected in
  the other direction.

### Every command answers

Each id-carrying command gets exactly one `result` — enforced *by
type*: the responder value can be consumed once, and dropping it
unanswered (a panicked handler, a code path that forgot) emits an
error result automatically. Malformed and unknown-typed commands
answer too, recovering the id from the envelope, so the host never
waits out its 30-second timeout on us. The same "degrade loudly" idea
governs the frame cap: an outbound frame over 4 MiB would be
*silently* skipped by the host, so we shrink it into an error result
or a `warn` instead.

Errors themselves are written to be read: a failed command's `result.
error` is a sentence the owner can see in-chat, and every failure
passes through one logged choke point so the stderr log tells the
story without correlating host logs.

### Deliver once, and to the right address

Message identity is the contract's backbone, and two invariants came
out of building against it (the hard way — both shipped wrong first):

- **Discard history once, recover downtime always.** Only the
  first-ever connect discards backlog (a freshly set-up bot must not
  replay pre-setup rooms into agent turns). Every later connect
  resumes from the persisted sync token, so messages sent while the
  connector was down arrive on reconnect — a silent restart gap reads
  as a broken bridge, not discipline. The residual is a one-batch
  at-least-once window at the crash boundary, documented and proposed
  for host-side dedupe.
- **The delivered-scope invariant.** The host correlates message
  events on the compound key `(chat_id, id)`, so an edit, delete, or
  reaction must carry the chat id its target message was *delivered*
  under — not merely a chat that contains it. Every delivery and send
  records its scope; every event-emitting path resolves through that
  record.

### Opaque ids are the extension point

connproto chat ids are opaque to the host — and that opacity is
load-bearing. Matrix threads become first-class chats by *codec*, not
by protocol change: a thread chat's id is `<room_id>;thread=<root>`,
a convention only this connector parses. The host sees a new chat of
kind `thread` and routes sends, asks, and speakers at it like any
other chat. The same trick resolves the host addressing the owner
"cold" by user id: a chat id that parses as an MXID resolves to the
`m.direct` DM with that user.

### Honest grades, bounded resources

- **Attestation by proof.** Ask answers are graded `attested` because
  a Matrix reaction is a signed event whose sender the origin
  homeserver authenticated — with the federation caveat documented
  (a hostile federated server can forge only its own users, and MXIDs
  embed their server, so exact-id `restrict_to` matching keeps grants
  sound). The grade follows what the platform proves, not what the
  widget looks like.
- **Everything is bounded.** Session caches are capped maps with
  eviction; oversized inbound lines are drained in constant memory;
  unable-to-decrypt warnings are burst-limited; command handlers carry
  timeouts inside the host's budget. A long-running session must not
  grow, and a hostile peer must not be able to make it.
- **Secrets discipline.** Tokens are masked in `status` output and
  never logged; session files are 0600 in a 0700 dir, written
  atomically; logs carry event ids, chat ids, kinds and sizes — never
  message content. Host-owned files (`pairing.json`, `data/`) are
  never touched, even by `reset`.

### Proven at three levels

Nothing is trusted to be conformant by construction:

1. **The golden corpus** — terva's own byte-exact golden frames,
   mirrored as tests, plus a byte-for-byte replay of the host's e2e
   conversation. The wire is right because the oracle says so.
2. **A mock homeserver** — every translation path exercised hermetically
   against wiremock, no network.
3. **A live throwaway Synapse** — the real binary driven end-to-end
   (headless setup, E2EE with real ciphertext on the wire, restarts,
   admission flows) with a second matrix-sdk client playing the human.
   Testing **never** touches a real Matrix account or homeserver; the
   throwaway (plus a preconfigured Element Web) exists so dogfooding
   doesn't either.

### The feedback loop is part of the design

Building the first real external connector doubles as a stress test of
the contract. Everything learned flows back as reviewable documents —
proposals, drafted contract text, and host-side reviews of both — kept
in `docs/` and cross-verified against the host source by the terva-side
agent. Two connector bugs and one host bug were found by exactly this
loop; the documents are ordered so the next reader can replay it.

## The components

| component | idea |
|---|---|
| **wire** | LF-JSON framing with the 4 MiB cap and bounded-memory drain; byte-compatible with the host's `lineframe` |
| **serve** | the protocol state machine: handshake, dispatch, the exactly-one-result responder, single-writer stdout, prompt teardown |
| **proto** | the frame types, golden-tested against terva's corpus |
| **matrix::client** | login/restore, the first-connect discard vs warm-resume decision, token validation |
| **matrix::rooms** | chat identity: dm/group shapes, the thread-id codec, the delivered-scope resolution |
| **matrix::inbound** | Matrix events → frames: messages, edits, redactions, reactions, membership, UTD warnings, invites |
| **matrix::outbound** | commands → Matrix: send (with optional per-message speaker profiles), edit, delete, react, threads, typing |
| **matrix::entities** | mention detection: pills, `m.mentions`, reply-to-bot, id-form-first span candidates |
| **matrix::media** | attachments both ways, by path through the host-owned `data_dir`; the SDK en/decrypts transparently |
| **matrix::asks** | the reaction widget: emoji legend + seeded reactions, attested answers, imposter filtering, expiry/close |
| **matrix::e2ee** | the verification story lives in `setup` (recovery keys, emoji/SAS); the session just decrypts and warns on UTD |
| **setup / verbs** | the operator surface: interactive login, masked status, reset-that-only-wipes-ours |

## Decisions and their reasons

- **matrix-sdk, pinned exact.** The native SDK gives E2EE, the sqlite
  stores, and sync for free; pinning `=0.18.0` keeps its fast-moving
  API surface stable under us, with every call verified against the
  registry sources.
- **Protocol 2 floor.** v1's id-in-reply_to conflation would poison the
  message-identity mapping everything above depends on; a floor is
  sanctioned when a wire feature is load-bearing, and this one is.
- **Asks as seeded reactions.** Matrix has no buttons. Seeding our own
  message with one reaction per option makes answering one tap on
  every client, the emoji legend doubles as the text fallback, and
  `ask_close`/expiry withdraw the furniture. MSC3381 polls remain a
  possible future upgrade.
- **Speaker profiles are config-gated.** MSC4144 per-message profiles
  render in some clients today; where they don't, the host's
  `**Name:**` prefix fallback is strictly better. So the connector
  declares `speaker:*` only when the operator opts in.
- **No automatic cross-signing setup.** Enabling it on an account with
  an existing identity is destructive; `setup` asks, and defaults to
  the safe paths (create-and-print a recovery key, or restore an
  existing one).
- **Dedupe stays host-side.** The connector could keep a delivered-ids
  cache, but the host is the natural owner (it already correlates on
  `(chat_id, id)` and survives connector restarts) — proposed upstream
  rather than papered over here.
