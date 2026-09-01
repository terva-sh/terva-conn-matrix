# Host-side review of `connproto-proposals.md`

Every claim in `connproto-proposals.md` checked against the terva host
source (its development trunk @ `a45196fb`). Line references are to
that tree.

Verdict: most of the document holds and several items are close to
free for the host to take. Four need correcting before they go
upstream, and one of those is a live bug in **this** connector rather
than a gap in terva.

---

## 1. Act on this first — §2.3 is our bug, not a doc gap

The proposal assumes *"the host correlates by message id and treats
`chat_id` as informative."* That is **false**. The host correlates on
the compound key `(chat_id, id)` everywhere:

| site | code |
|---|---|
| `loop.go:998` | `l.queue[i].ChatID == ev.ChatID && l.queue[i].ID == ev.ID` (edit) |
| `loop.go:1024` | `qm.ChatID == ev.ChatID && qm.ID == ev.ID` (delete) |
| `loop.go:1080-1096` | edit-note dedup keyed `chatID + "\x00" + id` |

So emitting a thread-resident message's edit under the *room* chat id
— which 0.10.0 does, because the Matrix edit event doesn't re-state
its thread — has consequences today:

- **Edits** miss the in-place queue rewrite and fall through to
  `addNote(ev.ChatID, …)` (`loop.go:1013`), so the
  `[chat event: message_edited]` note lands in the **room's**
  conversation — the wrong per-chat agent — while the thread's queued
  prompt keeps the stale text.
- **Deletes** never withdraw the queued message (`loop.go:1019-1035`),
  so the host runs a turn on a message the user visibly deleted. This
  is the sharp one.
- **Reactions** route their note by `ev.ChatID` too (`loop.go:1052`).

**Fix here, not upstream:** re-derive the thread scope for
`message_edited` / `message_deleted` / `reaction` on thread-resident
messages and emit the same `<room_id>;thread=<root>` chat id the
original `message` frame carried. The thread root is recoverable from
the target event's relation; where it isn't, the store already knows
which chat we delivered that message id under.

Still worth one sentence in the contract, but phrased as the rule it
actually is: *`chat_id` is load-bearing on every message event — emit
the same chat id the message was delivered under.*

---

## 2. Corrections to make before sending upstream

### §1.1 — right observation, wrong causal story

The mention did not raise the ask. `Loop.onMembership` (`loop.go:821`)
is the **only** site that fires an admission ask, and it is driven by
the `chat_membership added` frame — the invite — not by any message.
The mention was dropped independently at `gate.go:198-202`:
`routeGroup` returns `actHandled` for any unapproved chat, silent by
design ("THIS is the security boundary for group reach",
`admissions.go:36`).

The UX complaint is real and the remedy is still "buffer and replay",
but it must hang off **the gate's not-approved path**, not off "the
message that raised the ask" — no such coupling exists. Two things to
name in the proposal, because they are what a host maintainer will
push back on:

- Buffering holds untrusted content from a chat the owner has not
  approved and later injects it as a prompt. Bounded N, bounded age,
  and dropping the buffer on deny/expiry are load-bearing, not
  niceties.
- Replay must respect the admitted **mode**. Approving as `mention`
  should replay only the mentions, or the owner gets a burst they
  never consented to.

### §1.3 — the premises don't hold

- **Not silent.** All three carriers warn on overflow:
  `external/proxy.go:378`, `extconn/extconn.go:338`,
  `connlocal/connlocal.go:185` — all
  `"connector %q: inbound queue full; dropping a message"`. Under
  `terva bot run` the warn callback is nil (`botcmd.go:526`), so the
  line goes to stderr → `$TERVA_HOME/logs/bot.log`.
- **Not frames.** The 256 slot is a `chan chat.Message` — normalized
  inbound messages only, downstream of `handleFrame`.
  `chat_membership` never touches it (direct callback,
  `connhost.go:515-523`) and `answer` never touches it (per-ask
  buffered(1) channel where overflow is by-contract "first valid
  answer wins", `connhost.go:530-549`). Both failure examples the
  section gives are impossible.
- **"from the docs"** — `docs/connectors.md` never mentions the
  buffer. Whatever we coded defensively around, it was not documented
  there.

What survives is the actual ask: **aggregate the drops onto an
operator-visible surface**. Re-pitch it as "the log line exists but
nothing counts it", which is true and still worth fixing.

### §2.4 — three of the four are already written down

`docs/connectors.md:200` documents the 3-second hello, `:414` the 30s
send timeout, `:428` the ~2s SIGTERM escalation. The **connect budget
is the only undocumented one** (`external/proxy.go:27`) — and it is
the one the section says matters most and the one that cost us 0.7.0.

Lead with that specific gap instead of the general table. It becomes
the cheapest and most obviously-correct item in the document.

### §1.4 — correct, and cheaper than the proposal claims

Confirmed: `connsdk.go` `continue`s on a failed typed unmarshal for
every id-carrying command (`edit` :794, `react` :809, `delete` :824,
`thread_start` :839, `ask_close` :862), and `default:` (:877) logs to
errlog without emitting a result. The host then burns its full 30 s
`SendTimeout` per command.

Strengthen the ask with this: `connproto.Frame` **already carries
`ID`** (`connproto.go:56-59`) and is already parsed before the switch.
The envelope recovery is therefore free — a `w.result(frame.ID, "", err)`
per case plus one in `default:`, not a refactor.

---

## 3. Confirmed as written

**§1.2 (dedupe)** — no dedupe exists anywhere on the inbound path.
Two notes worth handing over with the proposal:

- The LRU must **not** live on `connhost.Session`. That session dies
  and is recreated on exactly the reconnect that motivates the
  proposal. The long-lived `inbound` channel owners survive redials
  (`external/proxy.go:110`, `extconn/extconn.go:307`); the `Loop`
  survives connector restarts entirely.
- The host already uses `(chat_id, id)` as its correlation key in two
  places (see §1 above), so a same-keyed LRU is architecturally
  consistent rather than a new concept.

**§2.1 (reconnect semantics)** — genuine gap. `docs/connectors.md`
says nothing about downtime, backfill, or first-connect history.

**§2.2 (attestation by proof, not widget kind)** — the contract text
is at `docs/connectors.md:268-277` and does read as a per-widget
prescription. The rephrase stands as proposed.

**§2.5 (idempotent re-announcement)** — the answer is **yes, within a
process run**: `onMembership` returns early for an already-approved
chat (`loop.go:843`), and the `admissionAsked` map (`loop.go:850-858`)
suppresses a second ask per chat. One caveat to fold into the ask:
`admissionAsked` is **in-memory only**, so after a host restart,
re-announcing `added` for a chat the owner previously *ignored* will
re-prompt them. Either ask for that state to be persisted, or scope
our re-announcement to chats we have not announced this session.

**§2.6 (golden corpus)** — correct, and it does pass by luck.
`TestGoldenFrames` is a byte comparison, and no golden frame's string
content contains `<`, `>`, or `&`.

**§3.1 (typing stop)** — correct: `TypingFromHost` is
`{type, chat_id}` only (`connproto.go:351-356`). Concrete hook to
cite: `Loop.startTyping` (`loop.go:463-480`) already returns a stop
closure, called via `defer` at `loop.go:359`. That is the single site
that would emit `active:false` — which makes this a genuinely small
change, not a new mechanism.

**§3.2 (answer retraction)** — no retraction path exists; accurate as
filed.

**§4 (what needed no change)** — the "emit optimistically, let the
host ignore" posture holds: `connhost.handleFrame` does not gate
inbound handling on the host's declared consume list.

---

## 4. Suggested reordering

By host-side cost-to-value:

1. **§2.4, narrowed to the connect budget** — one paragraph of docs.
2. **§1.4** — mechanical, and `Frame.ID` is already parsed.
3. **§1.2** — one bounded map, with the layering note above.
4. **§2.1 + §2.5** — the documentation pair; §2.5 arrives with its
   answer already known.
5. **§3.1** — small, with a single obvious call site.
6. **§1.1** — highest UX value, but carries real policy design.
7. **§1.3, demoted** — "add a counter", once the premises are fixed.
8. **§2.2, §3.2** — as filed.

**§2.3 moves out of "contract clarifications" entirely**: fix the
connector (§1 above), and file the one-sentence contract note
separately.
