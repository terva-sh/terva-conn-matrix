# connproto v2: field notes & proposals from the Matrix connector

Feedback for the terva host and the connproto contract, written after
shipping **terva-conn-matrix 0.10.1** — a complete external connector
(protocol 2, all optional features declared: entities, chat_membership,
edits/deletes/reactions both ways, attachments, asks, threads_out,
config-gated speaker) with a golden-corpus-conformant wire, a
mock-homeserver suite, and a live compliance harness that drives the real
binary against a throwaway Synapse across eight scenarios.

Everything below was hit for real — in implementation, in the live suite,
or in dogfooding with the real host — and has since been **verified
claim-by-claim against the terva source** (its development trunk @
`a45196fb`); the verification, with line references, lives in
[connproto-proposals-host-review.md](connproto-proposals-host-review.md).
The documentation-only items had their text drafted, placement-anchored,
in [connproto-doc-proposals.md](connproto-doc-proposals.md); every one
of those drafts has since landed, and the Status section below says
where each item here stands.
One original claim turned out to be a bug in this connector rather than a
gap in terva — see "Fixed on our side" at the end. Ordered by host-side
cost-to-value.

## Status (2026-08-31)

Verified item-by-item against terva `sothr-main` @ `f629e9e3` on
2026-08-30; §6 updated for terva #863 the day after. Nine of twelve
shipped — every documentation item, the host fix, the typing stop, the
admission-gate buffer, and the connsdk error results — and three did
not; of those, one (§3) is now named in the contract as
"not yet", and none is recorded upstream as declined. Each item below
opens with its own **Shipped** / **Not shipped** line saying where it
landed and how the shipped form differs from what was argued, or what
still holds it open.

| # | item | status |
|---|---|---|
| 1 | connect budget | shipped — terva **#616** (docs) |
| 2 | connsdk answers malformed/unknown | shipped — terva **#874** |
| 3 | host-side `(chat_id, id)` dedupe | **open** — the contract now says "does not yet" (#612) |
| 4 | reconnect semantics | shipped — terva **#612** (docs) |
| 5 | idempotent re-announcement | shipped — terva **#621** (docs; the rule as it stands, not persisted) |
| 6 | `typing` stop signal | shipped — terva **#863** (wire + host + connsdk), consumed in SDK **v0.3.4** and here in 0.14.0 |
| 7 | buffer at the admission gate | shipped — terva **#867** (host, no wire change) |
| 8 | inbound-queue drop counter | **open** — the bound is documented (#612), the count is not |
| 9 | attestation by proof | shipped — terva **#621** (docs) |
| 10 | golden corpus byte-exactness | shipped — terva **#623** (test guard); consumed in SDK **v0.3.1** |
| 11 | `answer` retraction | **open** |
| 12 | unpaired-run admission asks | shipped — terva **#606**, finished by `7d36e52d` |
| — | correlation-rule docs ("Fixed on our side") | shipped — terva **#617** |

Of the open three, §3 is the one the contract already names as "not
yet"; §8 is a counter, §11 a minor honesty fix. This table is a snapshot of one
tree, not a feed — re-verify against terva's development trunk before
citing it. (Trunk shas quoted in these documents are provenance for the
maintainers; terva's public mirror carries only its curated release
branch, so they will not resolve there.)

---

## 1. Document the connect budget *(one paragraph of docs)*

**Shipped — terva #616 (docs).** The `connect` budget is now in the
contract's timing prose: 30 s, and "budget that for the service dial and
auth round-trip only: push anything long-polling (an initial sync, a
gateway resume) into the session *after* `connected` goes out, or a
warm reconnect will quietly eat the whole budget and read as a hang" —
the exact bug that cost us until 0.7.0, stated as the reason for the
rule. Both expiry consequences landed with it (a first connect fails the
bridge; in crash recovery it spends one of three restarts per 60 s).
Prose, not a timing table — the table was declined (doc-proposals §2).

Of the four timing budgets we reverse-engineered, three are already in
`docs/connectors.md`: the 3 s `hello` (`:200`), the 30 s command send
timeout (`:414`), the ~2 s stdin-close→SIGTERM escalation (`:428`). The
**`connect` budget is the only undocumented one** (`external/proxy.go:27`)
— and it is the one that silently cost us: a warm-store Matrix sync
long-poll consumed the entire budget until 0.7.0 found it in a flaky
test. One paragraph in the contract's timing section surfaces that class
of bug in review instead.

## 2. connsdk: answer malformed and unknown commands *(mechanical)*

**Shipped — terva #874 (2026-09-01), exactly as scoped.** All nine
id-carrying command cases answer `result{id, "malformed <type>: …"}` off
the envelope on a failed typed unmarshal — the envelope survives the
body, as argued below — and the `default:` arm answers
`unknown command type` when the envelope carries an id, so a future
command degrades in milliseconds against an older connector instead of
per-command 30 s timeouts. Id-less frames stay log-only: no result is
owed, and inventing one would corrupt the correlation space.

The Go connsdk `continue`s on a failed typed unmarshal for every
id-carrying command (`edit` :794, `react` :809, `delete` :824,
`thread_start` :839, `ask_close` :862) and its `default:` arm (:877)
logs unknown command types without emitting a result. The host then
waits out its full 30 s send timeout for a result that will never come —
per command.

The fix is cheaper than it sounds: `connproto.Frame` **already carries
`ID`** (`connproto.go:56-59`) and is parsed before the switch, so the
envelope survives even when the body doesn't decode — recovery is one
error `result` per failing case plus one in `default:`, not a refactor.
The host learns in milliseconds instead of 30 s, and forward-compat with
future command types degrades gracefully. (Our serve loop has done this
since 0.2.0; the wire-smoke suite pins it.)

## 3. Dedupe inbound messages by `(chat_id, id)` — bless at-least-once connectors

**Not shipped — and the contract now says so.** The reconnect bullet
that shipped as terva #612 (§4) closes with: "On the boundary a
connector chooses between possibly delivering twice and possibly
dropping once; terva does not yet dedupe re-deliveries, so the contract
only names the trade-off — a duplicate is visible and recoverable, a
silently swallowed message is neither." At `f629e9e3` no `(chat_id, id)`
LRU exists in `chat/loop.go` or `connhost`; the only id-keyed collapse is
still the pre-existing edit-note dedup. "Not yet" is the upstream word,
so this is open rather than declined — and the trade-off clause already
makes at-least-once the blessed choice, which was the point.

The contract is silent on redelivery, so a connector must choose between
at-most-once (dogfood verdict: a restart cycle that silently eats
messages "is randomly silent instead of recovering" — reads as a broken
bridge) and at-least-once (risking double-delivery, the Matrix plan's
named worst failure mode). 0.10.0 recovers downtime by resuming the
store's sync token, which leaves an irreducible one-batch crash window
where a message could be delivered twice.

**Proposal:** for connectors declaring `message_ids`, the host keeps a
small per-chat LRU of recently seen message ids and drops duplicates
before they become agent turns. One bounded map host-side makes every
connector's recovery code trivially safe to write — at-least-once becomes
the obviously correct choice everywhere. Two implementation notes from
the review:

- The LRU must **not** live on `connhost.Session` — that object dies and
  is recreated on exactly the reconnect that motivates the proposal. The
  long-lived inbound channel owners survive redials
  (`external/proxy.go:110`, `extconn/extconn.go:307`), and the `Loop`
  survives connector restarts entirely.
- The host already correlates message events on `(chat_id, id)` in its
  queue rewrites (`loop.go:998`, `:1024`), so a same-keyed LRU is
  architecturally consistent rather than a new concept. (The same dedupe
  naturally covers `message_edited` re-delivery, which already collapses
  latest-wins.)

## 4. Specify reconnect semantics *(documentation)*

**Shipped — terva #612 (docs).** "**Reconnects recover, first connect
discards.**" landed as a bullet in `docs/connectors.md`, in the exact
shape we invented: discard history on the first-ever connect after
setup, deliver what arrived while down on every later connect, "bounded
by whatever 'recent' means on your platform", and "silently eating a
restart's worth of messages reads as a broken bridge, not as
discipline." Its closing clause names the at-least-once trade-off (§3).

The contract never says what a connector should do about messages that
arrived while it was down (`docs/connectors.md` says nothing about
downtime, backfill, or first-connect history). We invented: *discard
history exactly once (first connect after setup — never replay pre-setup
rooms into agent turns), recover downtime on every later connect, bounded
by whatever "recent" means on the platform.* If that is the intended
shape, write it into the contract so the next connector author doesn't
ship the at-most-once version first, as we did. Pairs with §3.

## 5. Bless idempotent `chat_membership` re-announcement *(docs; answer already known)*

**Shipped — terva #621 (docs; the "document the rule as it stands"
branch).** `docs/connectors.md`: re-announcing on reconnect is safe and
"is how you self-heal a frame terva missed"; a duplicate `added` for an
approved chat is a no-op, one for an already-asked chat does not re-ask,
and "that suppression is per host RUN and lives in memory, so the one
cost of over-announcing is that a chat the owner ignored will prompt
again after terva restarts." The suppression was not persisted —
`admissionAsked` is still an in-memory map (`loop.go:100`); the caveat
was documented instead, which is the alternative we offered.

`chat_membership added` is emitted exactly once per admission event; if
the host misses it (down, restart), the first-message fallback is the
only net. Could connectors cheaply self-heal by re-announcing current
membership on reconnect? The review answered it: **yes, within a host
process run** — `onMembership` returns early for an already-approved
chat (`loop.go:843`), and the `admissionAsked` map (`loop.go:850-858`)
suppresses a second ask per chat. The caveat: `admissionAsked` is
in-memory only, so after a **host** restart, re-announcing `added` for a
chat the owner previously *ignored* re-prompts them.

**Proposal:** either persist that suppression, or document the rule as
it stands: duplicate `added` is safe within a host run, so connectors
should scope re-announcement to chats not yet announced in their own
session (this connector already does — its announced set seeds from the
joined-room list at connect). See also §12 (now fixed): an undelivered
ask releases its claim, so re-announcement heals it; only an ask the
owner let time out stays burned for the run, deliberately.

## 6. `typing` stop signal *(small wire addition, one call site)*

**Shipped — terva #863 (2026-08-31), in the shape proposed plus one
gate.** `TypingFromHost` gained `active *bool` (`omitempty`; absent =
true), the golden corpus a `typing stop` case, `Loop.startTyping`'s stop
closure sends the frame — after the pulse goroutine has drained, so a
late refresh can never re-light what the stop cleared — and connsdk
routes it to a new optional `TypingStopper` transport. The gate we had
not argued for: the host sends the stop **only to a connector that
declared `typing_stop`**. A Go connector on an older connsdk unmarshals
the stop into the struct it knows, ignores the field, and asserts typing
once more — lengthening exactly the tail the frame exists to cut; the
"old connectors parse unchanged" claim below was true of the parse and
false of the effect. Consumed in terva-sdk-rust v0.3.4 (the field, the
refreshed corpus) and here in 0.14.0: we declare the feature and PUT
`typing: false` on the stop, and the live DM scenario proves the
indicator clears.

`typing` is fire-and-forget and refresh-based; there is no way to say
"stopped". On Matrix the indicator has a server-side timeout (we PUT
30 s and declare `typing_refresh_ms: 20000`), so after the agent's reply
lands, "tervabot is typing…" can linger for tens of seconds next to the
already-delivered answer. Every refresh-model platform has this tail.

**Proposal:** `{"type":"typing","chat_id":"…","active":false}` (absent
`active` = true, so old connectors parse unchanged — our null-tolerant
decoding would take it today). And it is genuinely small host-side:
`Loop.startTyping` (`loop.go:463-480`) already returns a stop closure,
`defer`-called at `loop.go:359` — that single site is where the stop
frame would go.

## 7. Buffer at the admission gate *(highest UX value; carries real policy design)*

**Shipped — terva #867 (2026-08-31), as proposed, with both constraints
kept load-bearing.** The gate holds an un-admitted chat's last messages
(`chat/held.go`: 5 per chat, 32 chats, 10 minutes, text and inline
images only — never staged files, which the receive path already
cleans up the moment the gate declines — commands excluded) and every
approval path releases them filtered by the admitted mode: `/approve`
in the chat, `/approve <id>` from the DM, and the membership ask, which
reaches the buffer through the Loop. Ignore, expiry, `/revoke`, and
removal drop it; an undelivered ask keeps it for the retry. Two things
the proposal did not say that the review of the host code added: edits
and deletes of a held message land on the held copy (the owner approves
what the chat says *now*, and a message its author took back never
becomes a turn), and the confirmation names what it is about to answer.
No wire change, so nothing here needed to move; DOGFOOD row 14 now
expects the reply.

**Observed:** invite the bot to a group, mention it; the host DMs the
owner for admission; the owner approves — and nothing happens. The
mention that triggered the whole flow is gone. The human's mental model
("I approved, so it should answer what I asked") breaks every time.

The mechanism (per the review): the admission ask is raised by the
`chat_membership added` frame — `Loop.onMembership` (`loop.go:821`) is
the only site that fires it — **not** by the mention. The mention died
independently at the gate's not-approved path (`gate.go:198-202`:
`routeGroup` returns `actHandled` for any unapproved chat, silent by
design — "THIS is the security boundary for group reach",
`admissions.go:36`). So there is no "message that raised the ask" to
hold onto; the remedy hangs off the gate.

**Proposal:** at the gate's not-approved path, buffer the last N
messages per un-admitted chat; inject them as the chat's first prompt on
approval; drop the buffer on deny/expiry. Two constraints are
load-bearing, not niceties:

- The buffer holds untrusted content from a chat the owner has not
  approved, and later injects it as a prompt — bounded N, bounded age,
  and dropping on deny/expiry are part of the security posture.
- Replay must respect the admitted **mode**: approving as `mention`
  should replay only the mentions, or the owner gets a burst they never
  consented to.

No wire change; pure host behavior.

## 8. Count inbound-queue drops *(a counter, not a mechanism)*

**Not shipped** (verified at terva `f629e9e3`, 2026-08-30).
The three warn sites still exist
(`extconn.go:338`, `external/proxy.go:407`, `connlocal.go:185`) and
nothing counts them; no operator surface reports drops. What did land is
the *bound* (terva #612: "terva buffers a few hundred normalized messages
… past that, overflow is dropped with a warning in terva's own log"), so
an author now knows the drop can happen. Making it visible after the
fact stays open.

Correcting our original filing: overflow of the 256-slot inbound queue
is **not** silent — all three carriers warn
(`"connector %q: inbound queue full; dropping a message"` —
`external/proxy.go:378`, `extconn/extconn.go:338`,
`connlocal/connlocal.go:185`; under `terva bot run` the line lands in
`bot.log`). And the queue holds normalized messages only:
`chat_membership` rides a direct callback (`connhost.go:515-523`) and
`answer` a per-ask buffered channel (`connhost.go:530-549`), so neither
can be lost there.

What remains is real: the warn line exists but **nothing counts it**.
With downtime recovery, a reconnect after a busy outage is exactly when
a burst arrives, and a lossy afternoon is invisible unless someone greps
the log. A drop counter on an operator surface (`terva bot status`)
would make it visible after the fact.

## 9. Grade attestation by what the platform proves, not by widget kind *(docs)*

**Shipped — terva #621 (docs).** The rule is now the principle: "grade
it by PROOF rather than by widget: `"attested"` when your platform
authoritatively proves who answered, and only then — button interactions
and callback queries qualify, and so does a signed event whose sender the
platform authenticates (a Matrix reaction does; document where your
platform lands)." The per-widget prescription is gone. The federation
caveat stays in our own docs; the contract keeps only the "document your
platform's caveat" clause.

The contract (`docs/connectors.md:268-277`) prescribes `"best_effort"`
for reactions as a category. On Matrix, a reaction is a signed event
whose sender the origin homeserver authenticated — identity-equivalent
to a button interaction, with one federation caveat (a hostile
*federated* server can forge only its own users, and MXIDs embed their
server, so exact-id `restrict_to` matching keeps grants sound). We
declared `attested` on that analysis.

**Proposal:** rephrase the rule as the principle it already implies:
*declare `attested` iff the platform cryptographically or
session-authoritatively proves the responder's identity; document your
platform's caveat.* Keep the examples, drop the per-widget prescription.

## 10. Golden corpus: byte-exactness vs JSON equivalence *(docs)*

**Shipped — terva #623, as a test rather than a comment — then consumed
on our side.** The corpus is published
(`packages/agent/connproto/testdata/golden.jsonl`) and
`TestGoldenCorpusIsEncoderNeutral` scans it for the *escapes* Go emits
(derived from `json.Marshal`, not hardcoded), with
`TestEncoderNeutralityGuardHasTeeth` proving the guard can fire — so
byte-exactness is now deliberate and enforced, not luck. `terva-connproto`
reads that file since SDK v0.3.1 (refreshed for the `dir` envelope in
v0.3.3); extproto's corpus followed (terva `1d922ae1`) and is consumed
since v0.3.2. The full account, including what the draft got wrong, is
doc-proposals §6.

Go's `encoding/json` HTML-escapes `<`, `>`, `&`; serde does not.
`TestGoldenFrames` is a byte comparison, and no current golden frame's
string content contains those characters — so byte-exact encode
comparisons pass by luck. Either keep the corpus free of those bytes
deliberately (a comment in `connproto_test.go` would do) or state that
conformance means canonical-JSON equivalence, not byte equality.

## 11. `answer` retraction *(minor)*

**Not shipped** (verified at terva `f629e9e3`, 2026-08-30).
`AnswerFromConn` (`connproto.go:288`) carries no
`removed`. Low priority then, low priority now; stays open.

Reaction-widget answers can be un-tapped; connproto has no way to
retract an answer (confirmed — no retraction path exists), so the host
may act on a tap the human visibly took back. Mostly harmless for
approve/deny (first answer usually decides), but a `removed: true` on
`answer` would make the widget semantics honest. Low priority.

## 12. Unpaired-run admission asks are burned and mis-addressed *(host fix)*

**Shipped — terva `7d36e52d` (2026-08-04), crediting this connector's
report.** Both defects are fixed, in a sharper shape than proposed: the
ask claim is taken only once the owner is reachable, and **released**
when the question fails to reach them — so re-announcement (§5) now
heals an undelivered ask, closing the hole the second bullet describes
without needing the connector-side mitigation. A **timeout still burns
the claim, deliberately**: the owner saw the question and let it
expire, and releasing on that would turn every ignored invite into a
nag on the next membership frame. The user-id seeding **stays** (it is
what lets a telegram bot open a conversation cold, and it self-corrects
on the first inbound DM); what changed is that nothing irreversible is
spent against a guessed chat id. Our 0.11.0 resolve-user-id-to-DM
mitigation remains correct and useful — it makes owner-directed frames
*deliverable* before the first DM, where the host fix only makes their
failure *recoverable*.

*Filed after the ordering above, from the
[doc-drafts review](connproto-doc-proposals-host-review.md) — by value
it belongs near the top; the number stays stable for the documents that
cross-reference this file.*

Two defects, source-verified, that compound:

- **The ask-suppression flag is written before the guard.**
  `admissionAsked[chatID] = true` lands at `loop.go:854`, *before* the
  `owner == "" || ownerDM == "" || asked` check at `:856` — so a
  `chat_membership added` arriving while the bot is unpaired burns that
  chat's one admission ask for the rest of the process run.
  Re-announcement (§5) cannot heal it: the duplicate hits the
  suppression, not the ask.
- **`pairedChatID` is seeded from the paired *user* id**
  (`loop.go:208-210`, under "for a DM connector the chat id is the user
  id") — a Telegram-shaped assumption. On Matrix an MXID is not a room
  id. Until the owner's first inbound DM of the run corrects it
  (`loop.go:247`), the admission ask (`:868`), tool approvals via
  `AskTarget` (`:626`), and the idle nudge (`:297`) are all addressed
  to a chat id the connector must reject.

**Combined failure:** invite the bot to a group before the owner's
first DM of that host run — the ask goes to an unusable chat id, fails,
is swallowed fail-closed (`loop.go:878-880`), and the chat is marked
asked for the rest of the run. The owner is never prompted, and no
reconnect or re-announcement recovers it. (This plausibly explains the
missing-admission-DM symptom our dogfood originally filed against
encrypted rooms — the encryption may have been coincidental to which
run the invite landed in.)

**Proposal:** move the `admissionAsked` write after the guard, so an
unprompted chat stays promptable; and seed `pairedChatID` only from a
real inbound DM — or let connectors declare that their user ids are not
chat ids.

---

## Fixed on our side: message events correlate by `(chat_id, id)`

**The docs half shipped — terva #617.** `docs/connectors.md` now
states: "**`chat_id` on these three frames is load-bearing.** terva
matches edits and deletes to queued and delivered messages on the
compound key (`chat_id`, `id`), and routes reaction notes by `chat_id`."
The consequence is split by frame type, per the host review — edits and
deletes miss the match, reactions are routed — where the draft below
treats all three alike. The threads clause landed as drafted ("…must
carry that same thread `chat_id`, per the correlation rule above").

We originally filed "state the correlation rule for message events" as a
clarification, assuming the host correlates by message id and treats
`chat_id` as informative. The review proved the opposite: the host
matches on the compound key everywhere (`loop.go:998` edits, `:1024`
deletes, `:1080-1096` edit-note dedup) — which made our thread handling
a live connector bug, not a doc gap. A thread-resident message's edit or
delete emitted under the *room* chat id missed the thread's queue: the
queued prompt kept its stale text, and a deleted message still became an
agent turn. **Fixed in 0.10.1**: edits, deletes, and reactions now carry
the chat id their target message was delivered under.

The text as drafted (what shipped differs as noted above), which was to
sit directly after the edits/deletes/reactions example block (near "Edits
always reference the ORIGINAL message id"):

> `chat_id` on `message_edited` / `message_deleted` / `reaction` is
> load-bearing: the host correlates these events with queued and
> delivered messages on the compound key **`(chat_id, id)`**. Emit the
> same `chat_id` the target message was originally delivered under —
> not merely a chat that contains the message. This bites platforms
> where one message is addressable in more than one scope (a thread
> and its parent room, say): the platform's edit/delete/reaction event
> often does not re-state the sub-chat, so the connector must remember
> — or re-derive — the delivered scope. A frame under the wrong chat id
> is not rejected; it silently misses the correlation, and a deleted
> message can still become an agent turn.

Plus one clause in the threads section, extending "messages inside it
arrive as ordinary `message` frames with the thread as their
`chat_id`": *…and edits, deletes, and reactions touching those messages
must carry that same thread `chat_id`.*

---

## What needed no change — patterns worth advertising

- **Opaque chat ids are load-bearing and excellent.** Matrix threads ride
  them as `<room_id>;thread=<root>` — a connector-private codec the host
  never sees. First-class thread support in the host would have been
  worse than this. Maybe advertise the pattern in the contract as the
  intended extension point for platform sub-chats.
- **`connect_error`-and-keep-serving for unconfigured connectors** makes
  the setup UX clean (the host reports "not configured" instead of a
  crash loop).
- **Attachments by path with host-owned `data_dir`** was exactly right
  for E2EE: the SDK decrypts on download and encrypts on upload, and the
  path handoff needed zero protocol awareness of any of it.
- **The 30 s host command timeout + per-command results** let all
  failure handling stay honest — every timeout/rate-limit surfaces as an
  error result string the owner can read in-chat.
- **The host consume list** (`hello_ack.capabilities.features`) is a good
  forward-compat valve; we emit optimistically and let the host ignore,
  which the contract permits (confirmed: `connhost.handleFrame` does not
  gate inbound handling on the declared consume list).

---

*Repo: `terva-sh/terva-conn-matrix` — the live harness (`just e2e`,
eight scenarios incl. E2EE, encrypted groups, asks, threads with
thread-scoped message events, downtime recovery) doubles as an
executable form of most claims above. Host line references:
[connproto-proposals-host-review.md](connproto-proposals-host-review.md).*
