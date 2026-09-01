# Graduation dogfood: the real host driving the whole surface

The live suite (`just e2e`) proves the connector side of the wire across
eight scenarios. This walkthrough proves the other half — the real terva
host driving this connector, with a human in a real client — over the
**whole shipped surface**: DM and E2EE, groups and admission, edits /
deletes / reactions, attachments, asks, threads, downtime and crash
recovery. It replaces the P2-era checklist, which predated most of that.

Default setting: the **throwaway Synapse** — nothing touches your Matrix
ecosystem; both accounts live only on it. The same checklist is the
graduation run against a **real homeserver**: point `setup` at it (after a
`reset`), use a dedicated bot account, and log the human side in from
your usual client. Only §1–§2 differ.

Estimated time: ~30 minutes for the whole table; §5.1–5.3 alone is 10.

## 0. Prerequisites

- `terva` on PATH, docker running (throwaway route), this repo checked out.
- A client that renders threads, reactions and edits — Element does (the
  bundled one below, or your own).

## 1. Start the throwaway homeserver (+ Element)

```bash
just synapse        # synapse http://127.0.0.1:18008, server_name: localhost
                    # element http://127.0.0.1:18009, already pointed at it
```

`just synapse-clean` later wipes it back to factory-fresh.

## 2. Create the two accounts

The bot account, via the API (registration is open on the throwaway):

```bash
curl -s -XPOST http://127.0.0.1:18008/_matrix/client/v3/register \
  -d '{"username":"tervabot","password":"tervabot-pw","auth":{"type":"m.login.dummy"}}' | head -c 200; echo
```

The human account: open **http://127.0.0.1:18009** → **Create account**
(any name, e.g. `drew`) — the bundled Element is preconfigured against
the throwaway, no homeserver editing, no email verification. For the
groups rows (§5.4) you want a **second** human account too — an
outsider who is not the owner.

## 3. Install and configure the connector

From this repo:

```bash
just link                                # terva bot link $(pwd)/connector.json
terva bot setup --connector matrix
#   homeserver URL: http://127.0.0.1:18008
#   user id:        tervabot
#   password:       tervabot-pw
#   e2ee:           [1] create a new recovery key (default — store the
#                   printed key; the throwaway makes it disposable) and
#                   answer n to "verify now" (or try the emoji flow by
#                   logging Element in AS tervabot and starting a
#                   verification against the "terva" device)
terva bot status                         # matrix block: MXID + masked token + e2ee
```

The connector state lands under `$TERVA_HOME/connectors/matrix/`:
`config.json` (the session bundle inside it, token values sealed
`enc:age:v2:…` once `terva secret init` has run), `secrets.key`, and
the sqlite state+crypto store. `terva bot reset --connector matrix` logs
the device out server-side and wipes it.

## 4. Run the bridge — with approvals ON

```bash
terva bot run --connector matrix --approval ask
```

A bot defaults to yolo (it runs its tools without asking); `--approval
ask` is what makes the **ask widget** fire (§5.6), and the admission
flow (§5.4) uses the same widget. From inside a running TUI,
`/connect matrix` works too, but the flags above are the daemon's.

Logs: `$TERVA_HOME/logs/connector-matrix.log` (or `terva bot logs
--follow` for a background `start`). Expect the startup banner (version,
protocol 2, state dir) and the connect path taken — `first connect:
pre-setup history discarded` this once, `warm start: resuming from the
store's sync token` on every later start.

## 5. The checklist

Pass criteria are written from the human's seat. Where a row says "the
agent mentions…", ask it — `what just happened in this chat?` is enough;
the host hands it chat events as bracketed `[chat event: …]` notes.

### 5.1 Pairing and DM basics

| # | check | pass looks like |
|---|---|---|
| 1 | Pair | from Element, start a DM with `@tervabot:localhost` and say hello; the bot auto-joins, the first sender claims the bridge (host-side — follow the confirmation it sends) |
| 2 | Round trip | your message becomes an agent turn; the reply lands in the DM |
| 3 | Markdown | ask for a bulleted list with a code block; it renders as formatting in Element, not as literal asterisks |
| 4 | Reply mapping | hover-reply to a bot message and ask a follow-up; the agent treats it as a normal prompt referring to that message, nothing mangled |
| 5 | Typing | "tervabot is typing…" shows while the agent works and **clears as the reply lands** (`typing_stop`, 0.14.0 + terva #863; on an older host it lingers up to ~30 s) |
| 6 | Built-ins | `/status` answers in-chat; `/stop` cancels a running turn |

### 5.2 Recovery

| # | check | pass looks like |
|---|---|---|
| 7 | Downtime recovery | Ctrl-C terva; send a DM while it's down; restart with the same command. The missed message **arrives and becomes a turn**, and fresh traffic flows. Only the first-ever connect after `setup` discards history; every later connect resumes from the store's sync token |
| 8 | Crash recovery | `kill -9` the `terva-conn-matrix` process (not terva); terva restarts it within seconds (watch the log); the conversation continues, nothing delivered twice |

Row 7 is the sharp one — PLAN.md §9 calls double-delivery the worst
failure mode, and 0.10.0 moved the boundary from "discard everything"
to "recover downtime". Recovery is bounded by the server's recent
window: a long outage delivers each room's most recent messages, not
the whole gap.

### 5.3 E2EE

| # | check | pass looks like |
|---|---|---|
| 9 | Encrypted DM | enable encryption in the DM (Element: room settings → Security & Privacy); the conversation keeps working both ways and Element shows the shield on both sides' messages |
| 10 | Encrypted downtime | repeat row 7 in the encrypted DM; the missed ciphertext decrypts on arrival (its room key rides the queued to-device traffic) |
| 11 | Unable-to-decrypt | (optional) log Element out and back in AS the human, send before keys re-share; the connector logs a UTD `warn`, never a crash, and recovers on the next message |

### 5.4 Groups and admission

Use the outsider account where it says "outsider".

| # | check | pass looks like |
|---|---|---|
| 12 | Invite → admission ask | create a named room and invite the bot; it auto-joins and **the owner's DM gets the admission ask**: a message with a legend and three seeded reactions — ① Approve (mention-only), ② Approve (all messages), ③ Ignore (the host sends no hints for these, so every option gets the circled-digit fallback) |
| 13 | Cold addressing | (optional) restart terva, and invite the bot to a fresh room **before** sending any DM in that host run; the ask still lands in the existing DM (a user-id-addressed frame resolves to the `m.direct` room — 0.11.0) |
| 14 | Approve | before tapping, have the outsider @-mention the bot in the group once (and send one plain line); then tap ① on the ask. The seeds are withdrawn, the outcome renders into the question message, the DM confirmation says "starting with the message that was waiting", and the bot **answers the mention** in the group — the plain line is not replayed (mention-only). terva #867 |
| 15 | Mention gate | in the group, a plain message does nothing; an @-mention of the bot (Element's pill) becomes an agent turn, attributed `@name:` |
| 16 | Outsider | the outsider @-mentions the bot in the admitted group; the agent answers (group reach), but the outsider's `/status` gets no answer — owner-only |
| 17 | Kick | remove the bot from the room; terva revokes the chat (a re-invite re-runs admission) |
| 18 | Encrypted group | repeat rows 12, 14, 15 in a room with encryption turned on before the invite; identical behavior, the mention arrives decrypted with its entity |

Held messages expire after 10 minutes and only the last 5 per chat are
kept (proposals §7) — approve within that window for row 14's replay.

### 5.5 Message events and attachments

| # | check | pass looks like |
|---|---|---|
| 19 | Edit before pickup | while the agent is busy on one message, send another and edit it before its turn starts; the agent answers the **edited** text |
| 20 | Edit after | edit a message the agent already answered; the agent mentions the edit on your next prompt |
| 21 | Delete queued | while the agent is busy, send a message and delete it before its turn; it **never becomes a turn** |
| 22 | Reaction as note | react 👀 to a bot message, then ask what happened; the agent mentions your reaction. Remove it; asked again, it knows it was removed |
| 23 | No bot echo | after §5.6 row 28, ask what chat events the agent saw; the bot's own seeded reactions, their withdrawal, and the outcome edit of its own message must **not** come back as notes (echo hygiene — the host drops them only because we return `result.message_id`) |
| 24 | Bot-originated events | the host `Loop` has no caller for outbound `edit`/`react`/`delete` yet (streaming edits are tracked host work) — nothing to check here; all three are pinned by the live suite |
| 25 | Inbound image | send a photo in the DM and ask what's in it; a vision-capable model describes it (file lands under the host's `data_dir`, never inline) |
| 26 | Inbound file | send a text file; the agent reads it with its tools |
| 27 | Oversize | (optional) set `max_attachment_mb: 1` in `config.json`, restart, send a larger file; it is dropped with a `warn` in the log and the text (if any) still arrives |

Outbound `send_image`/`send_file` are pinned byte-exact by the live
suite; here they show up only if your agent setup produces a file to
send (image generation, an extension that emits one).

### 5.6 Asks (approvals and questions)

Requires `--approval ask` (§4).

| # | check | pass looks like |
|---|---|---|
| 28 | Approval widget | ask the agent to run a shell command (`run \`date\``); the DM gets the approval question with a legend and three seeded reactions — 👍 Approve, ① Always (this tool) (no hint upstream, so the circled-digit fallback), 👎 Deny; tap 👍 → the tool runs, the seeds are withdrawn, `Approve — @you` renders into the question |
| 29 | Deny | again, tap 👎; the call is refused and the agent reports the denial |
| 30 | Expiry | again, ignore it; after the timeout the widget closes itself and the call is **denied** (fail-closed) |
| 31 | Attestation | again, tap ① (Always); the durable grant is **accepted** and the next call of that tool runs without asking — the host requires `attested` answers for allow-always, and a Matrix reaction is a signed event, so ours qualify |
| 32 | Imposter | in an admitted group, get the outsider to tap the owner-restricted approval's 👍 first; the tap is ignored (and redacted best-effort), the ask stays open for the owner |
| 33 | Agent question | ask the agent to ask you a multiple-choice question with 3 options; it arrives as the widget with ①②③-style hints; answer by reaction. Ask for a free-text or multi-select question; that one arrives as **numbered plain text** — deliberate (the floor is the richer path there) |

### 5.7 Threads

| # | check | pass looks like |
|---|---|---|
| 34 | Thread in | in Element, start a thread on a bot message and post in it; the reply arrives **in the thread**, and the thread is its own conversation (its context starts from the root snippet, not the room's history) |
| 35 | Thread events | edit, react to, and delete messages inside the thread; each behaves as in §5.5, scoped to the thread — the room conversation never sees them |
| 36 | Thread after restart | restart terva, edit a thread message sent before the restart; the edit still lands in the thread (re-derived from the event's relation). A **delete** of a pre-restart thread message degrades to the room — a known limitation (README) |

Outbound `thread_start` (`threads_out`) has no host caller yet — the
host `Loop` never opens threads today — so it is proven only by the
live suite's thread scenario.

### 5.8 Speaker profiles (optional)

Only visible when the host sends a speaker (personas / cast). Set
`"speaker": "name_only"` in `config.json`, restart; the bot's messages
carry `com.beeper.per_message_profile` and a client that renders
MSC4144 shows the speaker's name per message. Off, the host prefixes
`**Name:**` itself. Skip if you don't run personas.

### 5.9 Hygiene

| # | check | pass looks like |
|---|---|---|
| 37 | Status | `terva bot status` shows the matrix block with the token masked |
| 38 | Sealed at rest | with `terva secret init` done: `config.json`'s `session.access_token` reads `enc:age:v2:…`; the log never prints a token or message content |
| 39 | Reset | `terva bot reset --connector matrix` logs the device out (Element, as tervabot, no longer lists "terva") and empties `connectors/matrix/` |

## 6. Afterwards

```bash
terva bot reset --connector matrix   # log out + wipe connector state
just synapse-clean                   # wipe the homeserver
```

Anything that fails here is a bug in this repo unless a row says
otherwise — file the transcript from
`$TERVA_HOME/logs/connector-matrix.log` alongside what you saw in the
room. Anything that looks like a **host**-side gap goes to the ledger in
[docs/connproto-proposals.md](../docs/connproto-proposals.md) — check
there first; eight of its twelve items have shipped upstream.
