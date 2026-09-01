# Host-side review of `connproto-doc-proposals.md`

Companion to the [proposals review](connproto-proposals-host-review.md).
Every anchor, drafted clause, and open question in
[connproto-doc-proposals.md](connproto-doc-proposals.md) checked against
the terva host source (its development trunk @ `a45196fb`). Line
references are to that tree; `docs/connectors.md` unless stated.

Verdict: the drafts are in good shape. Items 1, 2, 3 and 5 land close to
as-written once the notes below are folded in; item 4 should be promoted
out of documentation entirely; item 6 has a stronger form worth taking.

Drafting the text rather than the intent worked — five of the six review
asks had a definite answer in the source, and the sixth surfaced a host
bug.

---

## 1. Answers to the six review asks

### Item 1 — does the consequence read true for reactions?

**No, and the blanket phrasing over-generalizes.** There is no
`(chat_id, id)` correlation for reactions at all: `OwnMessage` is
computed on message id alone (`connhost.go:142-146`, `s.ownMsgs[id]`),
so a wrong `chat_id` does not break the own-message determination. The
`chat_id` is purely the routing address for the resulting note
(`loop.go:1052`).

Split the consequence clause:

> …A frame under the wrong chat id is not rejected. For edits and
> deletes it silently misses the compound-key match — a queued message
> keeps its stale text, and a deleted message can still become an agent
> turn. For reactions the note is simply filed against the wrong
> conversation.

### Item 2 — what happens when the connect budget expires?

Two different outcomes, and the doc should say which applies when:

- **First connect.** `spawnAndConnect` fails → `shutdownChild()` → the
  error returns straight out of `Proxy.Connect`. The bridge fails
  immediately at `/connect` time with the error plus a pointer to
  `$TERVA_HOME/logs/connector-<name>.log`. The restart budget is not
  involved.
- **During crash recovery.** `restart()` → `spawnAndConnect` fails →
  the error is pushed back onto `childExit` (`external/proxy.go:210-216`),
  consuming one restart slot. Three in 60 s and `Receive` returns
  permanently ("process keeps crashing"). The deliberate-teardown
  detach (`external/proxy.go:316-318`) means a failed connect burns
  exactly one slot, not two.

`extconn`'s `redial` has the same shape and the same 3-in-60 s budget,
so one sentence covers both carriers:

> Blowing the budget fails the bridge immediately on first connect;
> during crash recovery it consumes one of the three restarts allowed
> per 60 seconds.

### Item 3 — is the at-least-once blessing safe to write?

**Not yet — soften it as drafted.** Host-side dedupe (proposals §3) has
not landed; there is nothing on the inbound path. Keep the bullet, name
the trade-off instead of prescribing the resolution, and tighten the
final clause when dedupe ships.

### Item 4 — documentation caveat, or persistence fix?

**Neither: it should be a host fix, and the drafted caveat undersells
the problem.** Two findings, both new since the first review:

- `admissionAsked[mb.ChatID] = true` is set at `loop.go:854`, **before**
  the `owner == "" || ownerDM == "" || asked` guard at `:856`. A
  membership event arriving while the bot is unpaired therefore burns
  the ask for that chat permanently within the process run.
  Re-announcement cannot self-heal it — which is precisely the case the
  drafted bullet advertises self-healing for.
- `pairedChatID` is seeded from the paired **user** id
  (`loop.go:208-210`), under the comment *"for a DM connector the chat
  id is the user id"* — a Telegram-shaped assumption. On Matrix an MXID
  is not a room id. Until the owner's first DM in a run corrects it
  (`loop.go:247`), the admission ask (`:868`), tool approvals via
  `AskTarget` (`:626`), and the idle nudge (`:297`) are all addressed to
  an MXID-as-chat-id.

Combined failure: invite the bot to a group before the owner has ever
DM'd it in that run, and the ask is addressed to an unusable chat id,
fails, is swallowed fail-closed (`loop.go:878-880`) — and the chat is
marked asked for the rest of the run. The owner is never prompted and
no reconnect recovers it.

This wants a new item in `connproto-proposals.md` (move the
`admissionAsked` write after the guard; seed `pairedChatID` only from a
real inbound DM, or let the connector declare that its user ids are not
chat ids). The documentation bullet stays, but its caveat should point
at the host item rather than describe a workaround.

### Item 5 — where does the federation caveat belong?

**In `connectors.md` as the one clause drafted, with the detail here.**
The contract should say *"document your platform's caveat"*; it should
not enumerate Matrix's federation model.

The replacement also fits the passage cleanly: the existing sentence
runs `:268-271` and ends at "…an inflated grade is a security lie", so
replacing up to the semicolon and keeping the durable-grants clause
works verbatim.

### Item 6 — comment, or canonical-JSON equivalence?

**Both are weaker than the real fix.** The corpus is *inline Go
literals* in `connproto_test.go`; there is no `testdata/` directory. If
external connectors are meant to treat it as a conformance oracle, they
are hand-copying literals out of a Go test file and drifting silently —
the escaping hazard is a symptom of that, not the disease.

The strongest form of this item: emit the corpus as a published
`testdata/*.jsonl` fixture that the Go test reads, which makes it
consumable cross-language *and* makes the escaping question concrete
(the fixture either contains those bytes or it doesn't). Keep the
drafted comment as the fallback if that is more than the host wants to
take on.

---

## 2. Wording and anchor corrections

- **Item 3** — "never replay messages that predate **the pairing**"
  conflates connector and host state. Pairing is host-side and the
  connector never sees it (`:433-435`). Say "predate that first
  connect."
- **Item 1** — anchor the threads addendum to `:401` ("messages inside
  it arrive as ordinary `message` frames"), not `:400-402`.
- **Anchors, two off-by-ones** — item 1's `:369 ff.` is `:368`; item 2's
  `:407` is `:408`.

---

## 3. Confirmed as drafted

Everything not called out above verifies:

- Item 1's main text and its `(chat_id, id)` justification
  (`loop.go:998`, `:1024`, `:1080-1096`).
- Item 2's `defaultConnectTimeout = 30 s` and its "service dial + auth
  round-trip" gloss (`external/proxy.go:27`).
- Item 3's premise — the contract is silent on downtime, backfill, and
  first-connect history.
- Item 4's core claim: duplicate `added` for a known chat is a no-op
  within a host run (`loop.go:843`, `:850-858`). Incomplete, per above,
  but not wrong.
- Item 5's diagnosis that the current text prescribes per-widget
  (`:268-271`).
- Item 6's escaping analysis: `TestGoldenFrames` is a byte comparison
  and no golden frame's string content contains `<`, `>`, or `&`.

---

*From `terva-sh/terva-conn-matrix`. The one item that changes scope is
4: it stops being a documentation caveat and becomes a host bug worth
its own entry in `connproto-proposals.md`.*
