# Drafted documentation updates for the connproto contract

Companion to [connproto-proposals.md](connproto-proposals.md): that
document argues *what* should change; this one drafts the actual text
for every **documentation-only** item, anchored to where it should land,
so review can happen on wording rather than intent. Anchors reference
the terva tree @ `a45196fb` (`docs/connectors.md` unless stated) — the
same tree the
[host-side review](connproto-proposals-host-review.md) verified against.
Every item has since landed (see Status), so those anchors are a record
of where each draft was aimed, not live line numbers — the host tree
has moved well past them.

The connector-side half of the correlation-rule documentation is already
landed in this repo (PLAN.md §3 states the invariant and the helper
discipline; README names it beside the thread-id convention).

Each item originally ended with an ask for the reviewer — usually
whether a consequence clause matched actual host behavior. The
[host-side review of these drafts](connproto-doc-proposals-host-review.md)
answered all six (and found a host bug along the way — now proposals
§12); items 1-6 below are **post-review**, with each item's resolution
noted in place.

Items **7-9** are newer: gaps the same review found in the contract
that no proposals entry covers. Items **10-12** are implementor
guidance — places the host source promises or permits something the
contract never states, found while reviewing it rather than by being
bitten.

## Status (2026-08-04)

**All twelve are landed in terva.** Each item keeps its draft below as
the record of what was argued; the **Shipped** line says where it went
and how the final text differs from the draft.

| # | item | status |
|---|---|---|
| 1 | `(chat_id, id)` correlation rule | shipped — terva **#617** |
| 2 | connect budget | shipped — terva **#616** (timing table declined) |
| 3 | reconnect semantics | shipped — terva **#612** |
| 4 | idempotent `chat_membership` re-announcement | shipped — terva **#621** (narrowed) |
| 5 | attestation graded by proof | shipped — terva **#621** |
| 6 | golden corpus byte-exactness | shipped — terva **#623** (fixture + guard); consumed in SDK **v0.3.1** |
| 7 | owner-directed addressing | shipped — terva **#608** |
| 8 | inbound queue is bounded | shipped — terva **#612** |
| 9 | carrier-dependent hello budget | shipped — terva **#616** |
| 10 | over-limit frames are skipped | shipped — terva **#621** |
| 11 | `result.message_id` powers echo hygiene | shipped — terva **#613** |
| 12 | where `warn` surfaces | shipped — terva **#613** |

The host bug these drafts turned up (proposals §12) shipped separately
as terva **#606** — the fix that made item 7's retry promise true —
and was finished by terva `7d36e52d` (2026-08-04): the ask claim is
now taken only once the owner is reachable and released when the
question fails to reach them, so only a timed-out ask stays burned
within a run (deliberately — the owner saw it).

Two decisions settled while landing, worth keeping consistent in any
future item:

- **Soft bounds, not constants.** Items 8 and 11 both describe a
  bounded host structure. Both shipped as "a few hundred" rather than
  the literal 256: a documented number becomes a compatibility surface
  connectors size logic against, and the host should stay free to
  retune.
- **Prose over tables.** Item 2's drafted timing table was declined.
  The other timings it would have gathered are already stated where
  they belong, and a four-row table in an otherwise-prose document buys
  organization at the cost of a second place to keep correct.

---

## 1. The `(chat_id, id)` correlation rule *(proposals: "Fixed on our side")*

**Shipped — terva #617.** Landed at the anchor below. The consequence clause shipped SPLIT by frame type, per the review: edits and deletes miss the compound-key match; reactions have no correlation at all and simply file their note against the wrong conversation. The threads corollary shipped as drafted.

**Where:** the edits/deletes/reactions block, directly after the "Edits
always reference the ORIGINAL message id…" paragraph (`:368`).

**Why:** the host correlates edits and deletes on the compound key
(`loop.go:998`, `:1024`, `:1080-1096`); nothing in the contract says so,
and this connector shipped a real bug (fixed in 0.10.1) on the wrong
assumption. A frame under the wrong chat id fails *silently* — the
sharpest failure being a deleted message still becoming an agent turn.

**Proposed text:**

> `chat_id` on `message_edited` / `message_deleted` / `reaction` is
> load-bearing: terva matches edits and deletes to queued and delivered
> messages on the compound key **(`chat_id`, `id`)**, and routes
> reaction notes by `chat_id`. Emit the same `chat_id` the target
> message was originally delivered under — not merely a chat that
> contains the message. This bites platforms where one message is
> addressable in more than one scope (a thread and its parent room,
> say): the platform's edit/delete/reaction event often does not
> re-state the sub-chat, so your connector must remember — or
> re-derive — the delivered scope. A frame under the wrong chat id is
> not rejected. For edits and deletes it silently misses the
> compound-key match — a queued message keeps its stale text, and a
> deleted message can still become an agent turn. For reactions the
> note is simply filed against the wrong conversation.

**Plus one clause in the threads section** (`:401`), extending
"messages inside it arrive as ordinary `message` frames with the thread
as their `chat_id`":

> …and edits, deletes, and reactions touching those messages must carry
> that same thread `chat_id`.

**Resolved:** the original blanket phrasing over-generalized — reactions
have no `(chat_id, id)` correlation at all (own-message determination is
by message id alone, `connhost.go:142-146`; `chat_id` only routes the
resulting note, `loop.go:1052`). The consequence clause above is split
accordingly, per the review's wording.

## 2. The connect budget *(proposals §1)*

**Shipped — terva #616.** The bullet landed with the 30s budget and both expiry consequences. The drafted timing TABLE was declined: the other three timings are already stated where they belong, and a table in a prose document is a second place to keep correct. Item 9 shipped alongside instead of as a table row.

**Where:** the first Rules bullet ("Answer `connect` with exactly one
`connected` or `connect_error`", `:408`).

**Why:** the only undocumented timing. `defaultConnectTimeout = 30 s`
(`packages/agent/chat/external/proxy.go`, "service dial + auth
round-trip"). A warm-store Matrix sync long-poll silently consumed the
entire budget until this connector's 0.7.0 found it in a flaky test —
in review, with the number written down, it would have been obvious.

**Proposed text** (extending the existing bullet):

> - Answer `connect` with exactly one `connected` or `connect_error` —
>   **within 30 seconds**. Budget it for the service dial + auth
>   round-trip only: push anything long-polling (an initial sync, a
>   gateway resume) into the session *after* `connected` goes out, or a
>   warm reconnect will eat the whole budget and read as a hang.
>   Blowing the budget fails the bridge immediately on first connect;
>   during crash recovery it consumes one of the three restarts allowed
>   per 60 seconds. `connect_error` means *permanently* broken (bad
>   token); transient network trouble is yours to retry inside the
>   session, surfaced via `warn`.

**Resolved:** the review traced both outcomes. First connect: the error
returns straight out of `Proxy.Connect` and the bridge fails at
`/connect` time with a pointer to the connector log — the restart
budget is not involved. During crash recovery: the failure is pushed
back onto `childExit` (`external/proxy.go:210-216`) and consumes
exactly one restart slot (the deliberate-teardown detach at `:316-318`
prevents double-counting); three in 60 s and the bridge is reported
broken. `extconn`'s `redial` has the same shape, so the one drafted
sentence covers both carriers.

## 3. Reconnect semantics *(proposals §4)*

**Shipped — terva #612**, with item 8 as its neighbour — that pairing was the point, since this bullet is what asks connectors to produce the burst item 8 bounds. The final clause still NAMES the trade-off rather than prescribing at-least-once; tighten it to a recommendation if proposals §3 (dedupe) ever lands.

**Where:** a new Rules bullet, after the send/result bullet (`:411`).

**Why:** the contract says nothing about downtime, backfill, or
first-connect history. We shipped the wrong (at-most-once,
discard-always) version first; the dogfood read it as a broken bridge.

**Proposed text:**

> - **Reconnects recover, first connect discards.** On the first-ever
>   connect after setup, discard history — never replay messages that
>   predate that first connect into agent turns. On every later
>   connect, deliver what arrived while you were down, bounded by
>   whatever "recent" means on your platform. On the boundary a
>   connector chooses between possibly delivering twice and possibly
>   dropping once; the host does not yet dedupe re-deliveries, so the
>   contract only names the trade-off — a duplicate is visible and
>   recoverable, a silently swallowed message is neither.

**Resolved:** dedupe (proposals §3) has not landed — nothing dedupes on
the inbound path — so the final clause now names the trade-off instead
of prescribing at-least-once; tighten it to a recommendation when §3
ships. Also per the review: "predate the pairing" conflated host and
connector state (pairing is host-side, invisible to the connector,
`:433-435`) — now "predate that first connect".

## 4. Idempotent `chat_membership` re-announcement *(proposals §5)*

**Shipped — terva #621, materially NARROWER than drafted.** The draft's closing caveat — that an ask burned while unpaired "cannot be healed by re-announcement at all (a host bug with its own fix pending)" — describes precisely the bug `7d36e52d` fixed. Landing it verbatim would have shipped documentation of a bug that no longer exists, and a future reader should not resurrect it from the draft below. What shipped is the verified remainder: duplicates are no-ops, the suppression is a per-run in-memory map, and the single cost of over-announcing is a re-prompt for an ignored chat after a host restart. The retry half was deliberately NOT restated — item 7's rule already carries it, and item 12 taught us adjacency beats repetition.

**Where:** the entities-and-membership block, after the
`chat_membership` example (`:344-352`).

**Why:** the review answered the open question — duplicate `added` is a
no-op within a host run (`loop.go:843`, `admissionAsked` `:850-858`) —
but the answer lives in nobody's docs, and the in-memory caveat decides
what a connector may safely do.

**Proposed text:**

> A duplicate `added` for a chat terva already knows is a no-op within
> a host run, so re-announcing current membership on reconnect is a
> safe way to self-heal a missed frame. Two caveats: after a **host**
> restart, re-announcing a chat the owner previously ignored will
> re-prompt them — so scope re-announcement to chats you have not
> announced in your own session — and an ask burned while the bot was
> unpaired cannot be healed by re-announcement at all (a host bug with
> its own fix pending).

**Resolved — and promoted:** the review found the drafted caveat
undersold the problem. The suppression flag is written *before* the
unpaired guard (`loop.go:854` vs `:856`), so a membership event from an
unpaired run burns the chat's ask permanently within the process — and
`pairedChatID` is seeded from the paired *user* id (`loop.go:208-210`),
which on Matrix is not a chat id, so early-run asks are mis-addressed
and swallowed fail-closed. That is a host bug, not contract text — now
**proposals §12**; the documentation bullet stays with its caveat
pointing there.

## 5. Attestation graded by proof, not widget kind *(proposals §9)*

**Shipped — terva #621**, essentially as drafted. One wording change: the draft's "iff" became plain prose ("when your platform authoritatively proves who answered, and only then"), since the biconditional survives being spelled out and reads less like jargon in a prose file. The rule now leads with the proof test and keeps the widgets as examples of it; the durable-grants clause is untouched.

**Where:** the asks block, the "Set `attestation` honestly" sentence
(`:268-271`).

**Why:** the current text prescribes per-widget ("best_effort for
parsed text or reactions"), but a Matrix reaction is a signed event
with an authenticated sender — the *principle* the examples encode is
about proof, not widget shape.

**Proposed text** (replacing the first sentence of the passage; the
durable-grants sentence stays):

> Set `attestation` honestly: `"attested"` iff your platform
> cryptographically or session-authoritatively proves who answered —
> button interactions and callback queries qualify, and so does a
> signed event whose sender the platform authenticates (a Matrix
> reaction; document your platform's caveat). `"best_effort"` for
> anything parsed out of text or otherwise spoofable.

**Resolved:** confirmed as drafted, and the splice is verbatim-clean —
the existing sentence runs `:268-271` and the durable-grants clause
("…an inflated grade is a security lie") survives unchanged. The
federation caveat stays in the connector's own docs; the contract keeps
only the "document your platform's caveat" clause.

## 6. Golden corpus: byte-exactness is by luck *(proposals §10)*

**Shipped — terva #623, in the preferred form PLUS a guard the draft did not propose.** The corpus is published at `packages/agent/connproto/testdata/golden.jsonl` (45 frames, one `{"name":…,"frame":…}` per line, frame carried as a JSON *string* so it survives the envelope), kept in lockstep with the in-source table by a test and regenerated with `-update-golden`.

**Our side is done too, in terva-sdk-rust v0.3.1.** `terva-connproto` vendors `golden.jsonl` into `tests/testdata/` and drives its suite from it: all 45 cases instead of the 12 hand-written functions' subset, with the decode tests keeping their value assertions but sourcing input by case name (a rename upstream panics rather than quietly testing nothing). A completeness test requires every case to be classified in exactly one direction, so a frame type terva adds tomorrow fails the suite instead of sitting unexercised, and `just golden-drift` (maintainer-only, since a path to a sibling terva must never reach the public tree) compares the copy to a checkout. The encode sweep round-trips the corpus value through our types, which pins shape rather than values — key order, an unmodelled field, and any `omitempty` divergence all fail; a changed fixture value does not, and should not.

The same argument then closed for extproto too: terva published its corpus (`1d922ae1`, 89 cases with a `dir` per line) and `terva-extproto` reads it since SDK **v0.3.2** — where the byte comparison caught a real divergence on its first run (Go writes a whole-number float64 as `3`, serde as `3.0`), which is exactly the kind of thing hand transcription had been hiding. terva then gave the connproto corpus the same `dir` envelope (`d83f651a`), consumed in SDK **v0.3.3**.

The escaping hazard is no longer guarded by a comment but by a test, and getting it right required correcting the draft twice over. The fallback comment we proposed — "keep golden string content free of `<`, `>`, `&`" — describes a check that **cannot ever fire**: Go escapes those characters before they reach a golden string, so scanning for the literal character matches nothing and the guard would pass forever while looking like coverage. The shipped guard scans for the *escapes*, and derives them from `json.Marshal` rather than hardcoding them, so it tracks the encoder and fails loudly if Go ever stops escaping. It was verified by injecting a hazardous frame into the real corpus — during which `TestGoldenFrames` passed throughout, which is exactly why this needed its own guard.

**Where:** not `connectors.md` — a comment in
`packages/agent/connproto/connproto_test.go`, at the golden-frame
comparison.

**Why:** Go's `encoding/json` HTML-escapes `<`, `>`, `&`; serde does
not. Every current golden frame is free of those bytes, so external
byte-exact comparisons pass — by accident, one test-fixture edit away
from breaking every non-Go connector's conformance suite.

**Proposed (preferred — the review's stronger form):** publish the
corpus as a `packages/agent/connproto/testdata/` JSONL fixture that
`TestGoldenFrames` reads. The corpus today is inline Go literals —
external connectors hand-copy them and drift silently; the escaping
hazard is a symptom of that, not the disease. A published fixture is
consumable cross-language, and it makes the escaping question concrete:
the fixture either contains those bytes or it doesn't.

**Fallback (the comment, if the fixture is more than the host wants):**

> ```go
> // These are BYTE comparisons, and external connectors treat this
> // corpus as their conformance oracle. Go's encoding/json
> // HTML-escapes <, >, & where other encoders (serde) do not — keep
> // golden string content free of those characters, or downgrade the
> // cross-language contract to canonical-JSON equivalence first.
> ```

**Resolved:** the review rated both original options (comment,
canonical-JSON equivalence) weaker than extracting the fixture; adopted
as the preferred form with the comment kept as fallback.

## 7. How terva addresses owner-directed frames *(docs half of proposals §12)*

**Shipped — terva #608.** Its open question resolved itself: #606 KEPT the user-id seeding (it is what lets a telegram bot open cold), so the drafted bullet was the contract rather than the simpler alternative. Two additions to the draft — connectors are told to resolve to an EXISTING DM rather than create one, and the retry promise is scoped to the admission ask, since tool approvals are simply denied when undeliverable.

**Where:** a new Rules bullet (`:406-431`), because it governs `send`
and `ask` alike — plus a correction to the idle-nudge passage at
`:601-605`.

**Why:** the behavior is proposals §12's second defect, and it needs
contract text whichever way the host fix goes. terva seeds the owner's
chat from the paired **user** id (`loop.go:208-210`) and corrects it
only on the owner's first inbound DM (`:247`); until then that value
addresses the admission ask (`:868`), tool approvals via `AskTarget`
(`:626`), and the idle nudge (`:297`).

Today this is documented *only* at `:601-602`, inside the idle-nudge
section ~190 lines below the protocol contract, phrased as a nudge
detail — a connector author writing `send`/`ask` handlers will not find
it there. Worse, `:604-605` then claims the nudge *"works with any
connector (`--connector matrix`, …) since the nudge lives in the
transport-agnostic loop"*. That is false wherever a user id is not a
valid chat id: the loop is transport-agnostic, the seeded chat id is
not.

**Proposed text** (new Rules bullet):

> - **Owner-directed frames may carry a chat id you never sent.** Until
>   the owner's first inbound DM in a host run, terva addresses
>   owner-directed frames — admission asks, tool approvals, the idle
>   nudge — using the paired **user** id as the `chat_id`, so the bot
>   can open a conversation cold. Where a user id is also a valid DM
>   chat id (telegram) this is free; where it is not (a Matrix MXID is
>   not a room id), either resolve it to the owner's DM chat or fail
>   the frame with a clear `result.error` — terva treats the failure as
>   fail-closed and the chat stays silent.

**Plus a correction** at `:604-605`, replacing "works with any connector
… since the nudge lives in the transport-agnostic loop":

> …works with any connector whose DM chat ids are user ids, or that
> resolves a user id to the owner's DM chat — see the addressing rule
> above.

**Was open:** draft against whichever shape proposals §12 takes, and land
only one. If the fix removes the user-id seeding entirely, this bullet
collapses to the much better *"terva never addresses a chat id it did
not first receive from you"* — and the `:604-605` correction becomes
unnecessary rather than merely narrower. If the seeding stays (it is
what lets a telegram bot open cold), the bullet above is the contract.

## 8. The inbound queue is bounded *(pairs with proposals §8)*

**Shipped — terva #612**, beside item 3. Two corrections to the draft: overflow warns in TERVA's output, not the connector's log (`connector-<name>.log` only ever carries our own stderr), and the bound is stated softly rather than as 256.

**Where:** the Rules list, immediately after the reconnect bullet
proposed in item 3 — flood pacing is that bullet's direct consequence.

**Why:** every carrier bounds the queue between the protocol session
and the chat loop at 256 (`external/proxy.go:38`,
`extconn/extconn.go:124`, `connlocal/connlocal.go:50`), and overflow
drops with a warn (`:378`, `:338`, `:185` respectively). Under
`terva bot run` the warn callback is nil (`botcmd.go:526`), so the line
lands in `$TERVA_HOME/logs/connector-<name>.log`. None of this appears
anywhere in `connectors.md`.

It matters precisely because item 3 tells connectors to *produce* a
reconnect flood. An author should know the far end is bounded — and,
equally, should not over-infer: the queue holds **normalized messages
only**. This is the misconception the first host review had to correct
in our own §8 draft.

**Proposed text:**

> - **Inbound messages queue, and the queue is bounded.** terva buffers
>   a few hundred normalized messages between your stream and the agent
>   loop; past that, overflow is dropped with a warning in your
>   connector log. Only `message` frames queue here — `result`,
>   `answer`, `chat_membership`, and the edit/delete/reaction streams
>   are dispatched directly and are never subject to it. Pace a large
>   recovery burst rather than emitting it in one batch.

**Was open:** two calls. Whether to state the number (256) — a documented
constant becomes a compatibility surface, and "a few hundred" may be
the better contract. And whether proposals §8's drop counter lands
first, in which case the bullet should name where an operator sees the
count.

## 9. The hello budget is carrier-dependent *(folds into item 2)*

**Shipped — terva #616.** Landed as a scoped sentence in place, not as a table row — see item 2. Numbers re-verified against `proxy.go` and `extconn.go` at the shipping commit.

**Where:** `:199`, "terva kills a child that sends no hello within 3
seconds."

**Why:** 3 s is the **standalone-process** budget
(`external/proxy.go:26`), and the comment there says why it is short:
*"A connector proxy is dialled on demand, not at startup, so its hello
grace stays short and independent of extdriver.DefaultHelloTimeout —
which was raised once extension loading moved off the startup path."* A
connector carried inside an extension gets 5 s for hello
(`extconn/extconn.go:113`) after up to 3 s of role registration
(`:112`). The word "child" scopes the current sentence by implication
only, and the connector-extensions section (`:607` ff.) never restates
the timing.

**Proposed text** (replacing the sentence):

> terva kills a spawned connector that sends no hello within 3 seconds.
> A connector carried inside an extension gets 5 seconds, measured
> after the extension registers the connector role (itself allowed 3
> seconds).

**Was open:** this is a row in the timing table item 2 asks for, not really
a standalone edit — if that table lands, scope its rows by carrier and
drop this item. Confirm the extension figures are current; they were
raised once already when extension loading moved off the startup path.

## 10. Over-limit frames are skipped, not fatal *(implementor guidance)*

**Shipped — terva #621.** The recovery semantics landed as drafted. Its own open question (below) resolved in the direction the draft guessed at but framed as a hedge: the cap is now stated as a FLOOR — "frames up to 4 MiB are accepted" — because for a sender the useful guarantee is what will be accepted, so a cap that rises keeps the promise while one that falls reads as the breaking change it is. Our `wire_smoke.rs` now pins behavior the contract promises.

**Where:** `:138`, "One JSON object per LF-terminated line; max line 4
MiB."

**Why:** the sentence states the cap and stops, so the natural reading
is "exceed it and the stream dies" — and an implementor codes
defensively against a failure mode that does not exist. The cap is
enforced **recoverably** (`connproto/frame.go:10-13`): an over-limit
frame is skipped, a warn fires, and the stream continues.
`NewFrameReader` "receives one message per skipped over-limit frame",
and both sides read through it — the host carrier
(`external/proxy.go:266`) and the Go SDK's stdin loop
(`connsdk.go:467`) — so the policy is symmetric and cannot drift per
site.

We reverse-engineered this from source; the tell that the contract
under-specifies it is that our wire-smoke suite has a dedicated
`oversized_inbound_frame_is_skipped_and_the_stream_recovers` test,
written to pin behavior the docs never promised.

**Proposed text** (extending the existing sentence):

> One JSON object per LF-terminated line; max line 4 MiB. The cap is
> recoverable, not fatal in either direction: a frame over the limit is
> **skipped with a warning and the stream continues**, so a single
> oversized attachment caption or a runaway text field costs you that
> one frame rather than the session. Do the same on your side — a
> reader that dies on an over-limit line turns a recoverable event into
> a reconnect.

**Was open:** confirm the cap is a promise rather than an
implementation detail. If a future carrier may lower it, the text
should say "at least 4 MiB" and name where the effective value is
announced; the recovery semantics are the load-bearing half either way.
Settled by shipping it as a floor — see the Shipped line above.

## 11. `result.message_id` is what powers echo hygiene *(implementor guidance)*

**Shipped — terva #613.** Landed beside the echo-hygiene promise it qualifies, not as a Rules bullet, which would have buried it. The bound is soft ("the most recent few hundred sends"), matching item 8.

**Where:** the host-defaults paragraph in the edits/deletes/reactions
block, at the "…are dropped host-side, so a connector that misses that
echo case is still covered" promise (`:383-385`).

**Why:** that promise has two conditions the contract never states.
`message_id` appears five times in the docs as a correlation field, and
nowhere as the thing that makes echo hygiene work:

- The host records it only from your `result` (`connhost.go:709` for
  `send`, `:789` for `ask`). A connector that omits `message_id` gets
  no own-message record at all — its own `ask_close` re-render comes
  back as "the user edited an earlier message", and **no reaction ever
  becomes a note**, because `loop.go:1042` drops every reaction whose
  `OwnMessage` is false.
- The record is a 256-entry ring (`connhost.go:120`). Past ~256 sends
  the oldest ids fall out and `isOwnMessage` returns false for them, so
  the safety net at `:467` / `:489` / `:507` stops covering.

The practical exposure is mostly the reaction-note downgrade — asks and
streaming edits touch fresh ids — but a stated guarantee with an
unstated bound is exactly what an implementor builds on and then gets
surprised by in a busy channel.

**Proposed text** (extending the promise):

> …are dropped host-side, so a connector that misses that echo case is
> still covered — **provided you return `result.message_id`**. That
> field is the only thing that tells terva which messages are the
> bot's: without it, your own ask-outcome re-renders read as user
> edits, and reactions never become notes at all (only reactions on the
> bot's own messages do). The record is bounded to the most recent few
> hundred sends per session, so the net covers recent messages, not
> your whole history.

**Was open:** whether to state the ring size (256) or "the most recent few
hundred", the same compatibility-surface question as item 8. Prefer one
answer across both items.

## 12. Where `warn` actually surfaces *(implementor guidance)*

**Shipped — terva #613.** Landed beside the `warn` frame in the reference. Sharper than drafted: both sinks were ALREADY documented 170 lines apart — the fix was adjacency, not new information.

**Where:** the frame reference, at the `warn` example (`:227`), or the
Rules bullet that says transient trouble is "surfaced via `warn`"
(`:411`) — one clause in either.

**Why:** the docs say to emit `warn` and never say who reads it. The
common assumption is that it reaches the user in-chat; it does not. A
`warn` frame goes to the host's operational output — the terminal for
`terva bot run`, `$TERVA_HOME/logs/bot.log` for `terva bot start`
(`:471`) — and never into a chat.

That is worth one clause because it sits next to a *different* sink the
docs do document: your process's own stderr goes to
`$TERVA_HOME/logs/connector-<name>.log` (`:93`). Both facts are
written down; nothing connects them, so an implementor cannot tell
which channel to use for what.

**Proposed text** (extending the `warn` bullet):

> `warn` frames are operator-facing, not user-facing: they surface in
> terva's own output (the terminal under `bot run`, `logs/bot.log`
> under `bot start`) and never reach a chat. Use `warn` for what an
> operator must see live — degraded auth, a reconnecting gateway,
> dropped attachments — and your own stderr (which lands in
> `logs/connector-<name>.log`) for everything else. Nothing you emit on
> either channel is visible to the humans in the chat.

**Was open:** none. This is additive and independent of every other item.

---

*From `terva-sh/terva-conn-matrix`. Items 1-6 are post-review, each with
its resolution in place. Items 7-9 come from the
[host-side review](connproto-doc-proposals-host-review.md) and cover
contract gaps with no proposals entry. Items 10-12 are **implementor
guidance** — not gaps we were bitten by, but places the host source
promises or permits something the contract does not say, found while
reviewing it.*

## What is left

Nothing on terva's side — all twelve are landed. Two things survive
this list, one of them ours.

**Ours: point our golden mirror at the published fixture.** Item 6 was
never really about escaping; the escaping hazard was a symptom. The
disease was that our golden corpus is a hand transcription of Go source
literals, and a transcription drifts silently — nothing anywhere fails
when the two diverge. The mirror lives in terva-sdk-rust's
`terva-connproto` now (this repo's `tests/golden.rs` moved there at SDK
P2/P3), so that crate is where a drift check against
`packages/agent/connproto/testdata/golden.jsonl` belongs — and its note
about byte-exactness holding by luck can go: it holds by a guard now.
One constraint: the fixture sits in the terva sibling checkout, which a
clean clone and the public mirror don't have — gate the check on an env
var naming the fixture path and skip when unset, rather than hardcoding
a relative path to the sibling checkout, which the release leak-scan
would rightly refuse.

**Theirs, and deliberate: item 3's final clause is provisional.** It
names the at-least-once/at-most-once trade-off rather than prescribing
a side, because terva does not dedupe re-deliveries. If proposals §3
lands, that clause should tighten into a recommendation — otherwise the
contract stays permanently vaguer than it needs to be, for a reason
nobody will remember.

## What the drafts got wrong

Worth keeping, because the pattern is more useful than the individual
corrections: **three of the four items landed in this last pass shipped
different from their draft, and in each case the draft was confidently
wrong rather than merely incomplete.**

- **Item 4** drafted a caveat describing a host bug that had since been
  fixed. Landing it verbatim would have documented a bug that no longer
  existed. Drafts written against a moving host go stale, and the
  staleness is invisible from inside the draft.
- **Item 6's** fallback comment specified a check that could never
  fire. It read as a reasonable guard and would have passed forever.
- **Item 10** hedged its own open question ("if a future carrier may
  lower it…") when the hedge was actually the right answer stated
  without confidence.

The common thread: each was caught by re-verifying against the host
source at landing time, not by re-reading the draft. A proposal is a
snapshot of someone's reading of a codebase on one day, and the
codebase is the thing that stayed true.
