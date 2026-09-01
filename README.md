# terva-conn-matrix

A **Matrix chat connector for [terva](https://terva.sh)** — a standalone
external connector speaking terva's connector protocol (**connproto v2**) over
stdio, written in Rust on the native
[matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk).

**Status: feature-complete (P8).** Password login and the verification
story (recovery keys, emoji/SAS) via `setup`, session restore into a
sqlite state+crypto store, a backlog-discarding sync loop, DM/group text
in and out — encrypted rooms transparently included, with
unable-to-decrypt warns — typing (with the `typing_stop` clear), invite
auto-join, `chat_membership` admission frames and `bot_mention` entities
for the host's group mention-gate, edits/deletes/reactions both ways,
typed attachments (ingest into `data_dir` + `send_image`/`send_file`),
approval asks as seeded-reaction widgets with attested answers,
work-stream threads as derived chats, and config-gated MSC4144
per-message speaker profiles, on a hardened, golden-corpus-conformant
connproto v2 wire (P1/P2). All eight plan phases are landed — see **[PLAN.md](PLAN.md)** and
**[CHANGELOG.md](CHANGELOG.md)**.

**Thread chat ids are a private convention**: a thread chat's id on the
connproto wire is `<room_id>;thread=<root_event_id>`. connproto chat ids
are opaque to the host, so only this connector ever parses them — nothing
else should learn to. One invariant rides on it: the host correlates
message events on `(chat_id, id)`, so edits, deletes, and reactions
touching a thread-resident message carry the same derived id the message
was delivered under (PLAN.md §3 has the rule and the mechanism).

## What this is

terva's bot mode bridges chat services to an agent through small, isolated
connector processes: the connector owns the service wire (auth, sync,
rendering), terva owns all policy (pairing, group admission, approvals,
sessions). One JSON object per LF line — host commands on stdin, service
events on stdout, logs on stderr. This repo is Phase 6 of terva's
chat-connectors plan: the Matrix connector, external-first.

## Layout

```
connector.json   — the terva connector manifest (name: matrix)
run.sh           — launcher terva execs; builds with cargo when sources changed
PLAN.md          — the implementation plan (protocol contract, Matrix mapping, phases)
src/
  main.rs        — verb dispatch (run/setup/status/reset/configured)
  lib.rs         — re-exports the connproto wire layer from terva-sdk-rust
                   (terva-connproto/-connsdk/-wire, git-pinned by release
                   tag) under the historical proto/serve/wire module names
  config.rs      — the matrix config + session persistence, on terva-env's
                   home resolution and 0600-atomic write discipline
  setup.rs       — the interactive/status verbs
  matrix/        — the service: client+sync (client.rs), room→chat mapping
                   (rooms.rs), inbound translation (inbound.rs), outbound
                   commands (outbound.rs), E2EE flows (e2ee.rs), mention
                   markup (entities.rs), attachments both ways (media.rs);
                   mock-homeserver tests (tests.rs)
tests/
  wire_smoke.rs  — drives the real binary through the host conversation
  verbs.rs       — configured/status/reset smokes against an isolated home
  conventions.rs — version lockstep + run.sh exec-bit guards
                   (the golden corpus + serve replay live with the SDK now)
docs/
  design.md      — the ideas: principles, components, decisions + reasons
  architecture.md— the code: process model, module map, data flows, invariants
  … and the connector↔host feedback conversation, in review order:
  connproto-proposals.md                  — proposals for the terva host
  connproto-proposals-host-review.md      — those, verified against host source
  connproto-doc-proposals.md              — drafted contract text (post-review)
  connproto-doc-proposals-host-review.md  — the review that shaped the drafts
```

## Development

Needs Rust ≥ 1.93 (matrix-sdk 0.18's MSRV) and [just](https://github.com/casey/just).

```bash
just test    # hermetic: mock-homeserver Matrix tests (wiremock), wire +
             # verb smokes, convention guards (the proto goldens and the
             # framing suite run in terva-sdk-rust, git-pinned by tag —
             # co-develop via the [patch] block in Cargo.toml)
just ci      # fmt + clippy -D warnings + locked tests
just link    # symlink connector.json into terva (dev install)
```

Live compliance testing runs against a **throwaway local Synapse** (docker,
`server_name: localhost`, open registration, no federation) — nothing
touches a real Matrix account or homeserver:

```bash
just e2e            # start the throwaway synapse + run tests/live_synapse.rs
just synapse        # the server (http://127.0.0.1:18008) + a bundled Element
                    # Web (http://127.0.0.1:18009) already pointed at it
just synapse-clean  # stop it and wipe its state (factory-fresh next start)
```

The live suite drives the real binary end-to-end — headless `setup`,
session restore, invite auto-join with DM detection, the text round trip
(markdown + rich replies), typing, the backlog-discard restart rule, the
encrypted-DM scenario (ciphertext proven on the wire), the group scenario
(admission frames + mention entities), and the message-events scenario
(edits/deletes/reactions both ways + attachments byte-exact through
`data_dir`) — with a second matrix-sdk client playing the human.

Installed (non-dev) copies live at `$TERVA_HOME/connectors/matrix/` — the
directory basename must equal the manifest name for discovery.

Development happens on an internal trunk;
[github.com/terva-sh/terva-conn-matrix](https://github.com/terva-sh/terva-conn-matrix)
carries the curated `release` branch and the version tags. The tree
there pins the public
[terva-sdk-rust](https://github.com/terva-sh/terva-sdk-rust) mirror by
the same tag names, so it builds with no access to anything internal.

## Setup against a real homeserver

Dedicate a Matrix account to the bot (its device sends and reads as that
account; don't share your own). Then:

```bash
just link                                # or unpack a `just dist` archive and
                                         # `terva bot link <dir>/connector.json`
terva bot setup --connector matrix
#   homeserver URL: https://matrix.example.org
#   user id:        botaccount            (localpart or full @botaccount:server)
#   password:       …
#   e2ee:           [1] create a new recovery key — STORE THE PRINTED KEY —
#                   or [2] restore the account's existing one. Optionally
#                   verify the new "terva" device via emoji/SAS from another
#                   logged-in client when prompted.
terva bot status                         # MXID + masked token + e2ee summary
terva bot run --connector matrix         # or `terva bot start` for background
```

DM the bot to pair; invite it to rooms and mention it (the host's
admission flow gates every group). `terva bot reset --connector matrix`
logs the device out server-side and wipes the local state.

Config knobs in `$TERVA_HOME/connectors/matrix/config.json`:

- `auto_join`: `"always"` (default) or `"never"` — whether invites are
  accepted at the Matrix layer at all.
- `max_attachment_mb` (default 64): inbound attachment ceiling; larger
  files are dropped with a `warn`.
- `speaker`: `"off"` (default) / `"name_only"` / `"full"` — MSC4144
  per-message cast profiles (`com.beeper.per_message_profile`). Off, the
  host prefixes `**Name:**` itself, which renders everywhere; turn it on
  once your clients render per-message profiles (`full` also uploads
  per-speaker avatars).

Release archives: `just dist` produces
`dist/terva-conn-matrix-<version>-<target>.tar.gz` containing the
prebuilt binary, manifest, launcher, and docs — the unpacked directory
links on a machine with no Rust toolchain (`run.sh` prefers the bundled
binary).

The protocol contract lives in the terva repo: `docs/connectors.md` and
`packages/agent/connproto/` (its golden frame tests are the conformance
oracle this repo's proto tests mirror). Feedback flowing the other way —
host fixes, contract clarifications, and small wire proposals learned by
building this connector — lives under `docs/` (see the layout above):
start at **[docs/connproto-proposals.md](docs/connproto-proposals.md)**;
each document has a host-side review companion written against the terva
source.

## Known limitations

- **Typing tail on older hosts**: Matrix typing indicators are
  timeout-based (we PUT 30 s; the host refreshes every 20 s). We declare
  `typing_stop` and clear the indicator on the host's stop frame (terva
  #863), so on a host that speaks it "…is typing" ends with the reply;
  on an older host it can still linger up to ~30 s after the reply
  lands — the host never sends what we declared to a connector that
  did not declare it, and vice versa.
- **Owner addressed by user id, no DM yet**: until the owner's first DM
  of a host run, terva addresses admission asks/approvals/nudges by the
  paired user id (proposals §12); we resolve that to the existing
  `m.direct` DM — if no DM exists yet, the frame fails with a diagnostic
  pointing at §12.
- **Redactions of pre-restart thread messages** degrade to the room
  chat id: a redacted event has its relations stripped and the
  delivered-scope cache is session-local (CHANGELOG 0.10.1).
- **Downtime recovery is bounded by the homeserver's recent window**: a
  long outage delivers each room's most recent messages, not the whole
  gap, and the crash boundary is at-least-once for one sync batch until
  the host dedupes (proposals §3).

## License

[MIT](LICENSE)
