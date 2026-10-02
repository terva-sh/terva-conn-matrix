//! Room → connproto chat mapping (PLAN.md §3, "Identity & chats"), and the
//! derived thread-chat id codec (feature threads_out).
//!
//! Matrix threads are relation trees inside a room, not first-class chats,
//! so a thread chat rides connproto's opaque chat ids as
//! **`<room_id>;thread=<root_event_id>`** — a convention private to this
//! connector (documented in the README; the host never parses chat ids).

use std::time::Duration;

use matrix_sdk::config::RequestConfig;
use matrix_sdk::ruma::events::relation::Reply;
use matrix_sdk::ruma::events::relation::Thread;
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContentWithoutRelation};
use matrix_sdk::ruma::{EventId, OwnedEventId, RoomId, UserId};
use matrix_sdk::{Room, RoomDisplayName, RoomState};

use super::Shared;

const THREAD_MARKER: &str = ";thread=";

/// A resolved outbound target: the room, plus the thread root when the
/// wire chat id was thread-derived.
pub(crate) struct ChatTarget {
    pub room: Room,
    pub thread_root: Option<OwnedEventId>,
}

pub(crate) fn thread_chat_id(room_id: &RoomId, root: &EventId) -> String {
    format!("{room_id}{THREAD_MARKER}{root}")
}

/// Split a wire chat id into its room part and optional thread root.
pub(crate) fn split_chat_id(chat_id: &str) -> (&str, Option<&str>) {
    match chat_id.split_once(THREAD_MARKER) {
        Some((room, root)) => (room, Some(root)),
        None => (chat_id, None),
    }
}

/// `(chat_kind, chat_title)` for a room. A chat is a `dm` when `m.direct`
/// account data says so, or when we learned it from an invite's `is_direct`
/// flag this session; `group` otherwise. DMs carry no title (the peer's name
/// is the host's business); Matrix has no strong `channel` analog.
pub(crate) async fn chat_shape(shared: &Shared, room: &Room) -> (String, String) {
    let dm = match room.is_direct().await {
        Ok(direct) => direct,
        Err(err) => {
            tracing::warn!("is_direct failed for {}: {err}", room.room_id());
            false
        }
    } || shared
        .dm_rooms
        .lock()
        .expect("dm set lock")
        .contains(room.room_id());
    if dm {
        return ("dm".into(), String::new());
    }
    let title = match room.display_name().await {
        Ok(RoomDisplayName::Empty) => String::new(),
        Ok(name) => name.to_string(),
        Err(err) => {
            tracing::warn!("display_name failed for {}: {err}", room.room_id());
            String::new()
        }
    };
    ("group".into(), title)
}

/// Resolve an outbound `chat_id` — plain room or thread-derived — to a
/// joined room and its optional thread root.
pub(crate) fn resolve_chat(shared: &Shared, chat_id: &str) -> Result<ChatTarget, String> {
    let (room_part, thread) = split_chat_id(chat_id);
    let room = resolve_joined_room(shared, room_part)?;
    let thread_root = thread
        .map(|root| {
            EventId::parse(root)
                .map(|id| id.to_owned())
                .map_err(|err| format!("bad thread root in chat id {chat_id:?}: {err}"))
        })
        .transpose()?;
    Ok(ChatTarget { room, thread_root })
}

/// Resolve a plain room id to a joined room. A chat id that is a USER id
/// is the host addressing the owner cold: until the owner's first inbound
/// DM of a host run, terva addresses admission asks, tool approvals, and
/// the idle nudge by the paired user id (connproto-proposals §12) — per
/// doc-proposals item 7's addressing rule, we resolve it to the existing
/// DM room with that user, and fail with a pointer when none exists.
pub(crate) fn resolve_joined_room(shared: &Shared, chat_id: &str) -> Result<Room, String> {
    let room_id = match RoomId::parse(chat_id) {
        Ok(room_id) => room_id,
        Err(err) => match UserId::parse(chat_id) {
            Ok(user_id) => return resolve_dm_with(shared, &user_id),
            Err(_) => return Err(format!("bad chat id {chat_id:?}: {err}")),
        },
    };
    let room = shared
        .client
        .get_room(&room_id)
        .ok_or_else(|| format!("no such chat: {chat_id}"))?;
    if room.state() != RoomState::Joined {
        return Err(format!("not joined to chat {chat_id}"));
    }
    Ok(room)
}

/// The joined DM room with `user_id`, per `m.direct` account data.
fn resolve_dm_with(shared: &Shared, user_id: &UserId) -> Result<Room, String> {
    match shared.client.get_dm_room(user_id) {
        Some(room) if room.state() == RoomState::Joined => {
            tracing::info!(
                user = %user_id,
                room = %room.room_id(),
                "user-id chat id resolved to the DM room (owner addressed cold — proposals §12)"
            );
            Ok(room)
        }
        _ => Err(format!(
            "chat id {user_id} is a user id, not a room id — the host addressed an \
             owner-directed frame by user id (connproto-proposals §12) and no joined \
             DM with that user exists yet"
        )),
    }
}

/// The chat identity + reply mapping of one inbound message event.
pub(crate) struct InboundShape {
    pub chat_id: String,
    pub chat_kind: String,
    pub chat_title: String,
    pub parent_chat_id: String,
    pub parent_chat_kind: String,
    /// Set only for REAL replies — a thread relation's `is_falling_back`
    /// target is rendering compatibility, not intent.
    pub reply_to: String,
}

/// Route an inbound message into its chat: an `m.thread` relation puts it
/// in the derived thread chat (kind `thread`, title = root snippet);
/// anything else lands in the room with the P2 dm/group shape.
pub(crate) async fn inbound_shape(
    shared: &Shared,
    room: &Room,
    relates_to: Option<&Relation<RoomMessageEventContentWithoutRelation>>,
    event_id: &EventId,
) -> InboundShape {
    if let Some(Relation::Thread(thread)) = relates_to {
        let (parent_chat_kind, _) = chat_shape(shared, room).await;
        let root = &thread.event_id;
        note_thread_event(shared, root, event_id);
        let reply_to = thread
            .in_reply_to
            .as_ref()
            .filter(|_| !thread.is_falling_back)
            .map(|r| r.event_id.to_string())
            .unwrap_or_default();
        return InboundShape {
            chat_id: thread_chat_id(room.room_id(), root),
            chat_kind: "thread".into(),
            chat_title: thread_title(shared, room, root).await,
            parent_chat_id: room.room_id().to_string(),
            parent_chat_kind,
            reply_to,
        };
    }
    note_room_event(shared, event_id);
    let reply_to = match relates_to {
        Some(Relation::Reply(reply)) => reply.in_reply_to.event_id.to_string(),
        _ => String::new(),
    };
    let (chat_kind, chat_title) = chat_shape(shared, room).await;
    InboundShape {
        chat_id: room.room_id().to_string(),
        chat_kind,
        chat_title,
        parent_chat_id: String::new(),
        parent_chat_kind: String::new(),
        reply_to,
    }
}

/// The `m.relates_to` for outbound content into `target`. In a thread chat
/// everything carries the thread relation — an explicit `reply_to` becomes
/// a real in-thread reply, otherwise the `is_falling_back` fallback points
/// at the latest in-thread event we know (root when we know none). In a
/// plain chat, `reply_to` becomes a rich reply.
pub(crate) fn outbound_relation(
    shared: &Shared,
    target: &ChatTarget,
    reply_to: &str,
) -> Option<Relation<RoomMessageEventContentWithoutRelation>> {
    let reply_target = if reply_to.is_empty() {
        None
    } else {
        match EventId::parse(reply_to) {
            Ok(event_id) => Some(event_id.to_owned()),
            Err(err) => {
                // A bad reply target degrades to a plain (or thread) send.
                tracing::warn!("ignoring bad reply_to {reply_to:?}: {err}");
                None
            }
        }
    };
    match (&target.thread_root, reply_target) {
        (Some(root), Some(reply)) => Some(Relation::Thread(Thread::reply(root.clone(), reply))),
        (Some(root), None) => {
            let latest = shared
                .thread_latest
                .lock()
                .expect("thread latest lock")
                .get(root)
                .cloned()
                .unwrap_or_else(|| root.clone());
            Some(Relation::Thread(Thread::plain(root.clone(), latest)))
        }
        (None, Some(reply)) => Some(Relation::Reply(Reply::with_event_id(reply))),
        (None, None) => None,
    }
}

/// Record `event_id` as the latest event of its thread (feeds the reply
/// fallback of the next threaded send) and remember it was delivered under
/// the thread's derived chat.
pub(crate) fn note_thread_event(shared: &Shared, root: &EventId, event_id: &EventId) {
    shared
        .thread_latest
        .lock()
        .expect("thread latest lock")
        .insert(root.to_owned(), event_id.to_owned());
    shared
        .thread_scopes
        .lock()
        .expect("thread scopes lock")
        .insert(event_id.to_owned(), Some(root.to_owned()));
}

/// Remember that `event_id` was delivered (or sent) room-scoped, so message
/// events about it resolve their chat id without a server round trip.
pub(crate) fn note_room_event(shared: &Shared, event_id: &EventId) {
    shared
        .thread_scopes
        .lock()
        .expect("thread scopes lock")
        .insert(event_id.to_owned(), None);
}

/// The chat id a message event (edit/delete/reaction) about `target` must
/// carry. The host correlates these on `(chat_id, id)`, so it must be the
/// chat id `target` was DELIVERED under — the derived thread chat when the
/// message rode a thread, the room otherwise. Events seen this session
/// answer from the scope cache; unknown ones (a pre-restart message edited
/// today) fall back to fetching the target and reading its thread relation.
pub(crate) async fn event_chat_id(shared: &Shared, room: &Room, target: &EventId) -> String {
    let cached = shared
        .thread_scopes
        .lock()
        .expect("thread scopes lock")
        .get(&target.to_owned())
        .cloned();
    let scope = match cached {
        Some(scope) => scope,
        None => fetch_thread_scope(shared, room, target).await,
    };
    match scope {
        Some(root) => thread_chat_id(room.room_id(), &root),
        None => room.room_id().to_string(),
    }
}

/// Cache-only variant for redaction targets — a redacted event has its
/// relations stripped, so there is nothing left to fetch. An unknown event
/// degrades to room-scoped.
pub(crate) fn cached_event_chat_id(shared: &Shared, room: &Room, target: &EventId) -> String {
    let scope = shared
        .thread_scopes
        .lock()
        .expect("thread scopes lock")
        .get(&target.to_owned())
        .cloned()
        .flatten();
    match scope {
        Some(root) => thread_chat_id(room.room_id(), &root),
        None => room.room_id().to_string(),
    }
}

/// Fetch `target` and read its thread relation. Only `m.room.message`
/// rides threads in our translation (a threaded sticker is delivered
/// room-scoped); an encrypted envelope carries its relation in cleartext.
/// A successful fetch caches its answer either way; a failed one caches
/// nothing — it must not pin a thread message as room-scoped.
async fn fetch_thread_scope(
    shared: &Shared,
    room: &Room,
    target: &EventId,
) -> Option<OwnedEventId> {
    let fetch = room.event(
        target,
        Some(RequestConfig::new().timeout(Duration::from_secs(3))),
    );
    let fetched = match tokio::time::timeout(Duration::from_secs(4), fetch).await {
        Ok(Ok(event)) => {
            let raw = event.raw();
            let kind = raw
                .get_field::<String>("type")
                .ok()
                .flatten()
                .unwrap_or_default();
            let root = (kind == "m.room.message" || kind == "m.room.encrypted")
                .then(|| raw.get_field::<serde_json::Value>("content").ok().flatten())
                .flatten()
                .and_then(|content| {
                    let relates = &content["m.relates_to"];
                    if relates["rel_type"] != "m.thread" {
                        return None;
                    }
                    relates["event_id"]
                        .as_str()
                        .and_then(|id| EventId::parse(id).ok())
                });
            Some(root)
        }
        Ok(Err(err)) => {
            tracing::debug!("thread scope fetch for {target} failed: {err}");
            None
        }
        Err(_) => {
            tracing::debug!("thread scope fetch for {target} timed out");
            None
        }
    };
    if let Some(scope) = &fetched {
        shared
            .thread_scopes
            .lock()
            .expect("thread scopes lock")
            .insert(target.to_owned(), scope.clone());
    }
    fetched.flatten()
}

/// Best-effort thread title: a snippet of the root event's body, cached.
/// Failures cache as empty — a title is decoration, not worth refetching
/// per message.
pub(crate) async fn thread_title(shared: &Shared, room: &Room, root: &EventId) -> String {
    if let Some(title) = shared
        .thread_titles
        .lock()
        .expect("thread titles lock")
        .get(&root.to_owned())
        .cloned()
    {
        return title;
    }
    let fetch = room.event(
        root,
        Some(RequestConfig::new().timeout(Duration::from_secs(3))),
    );
    let title = match tokio::time::timeout(Duration::from_secs(4), fetch).await {
        Ok(Ok(event)) => event
            .raw()
            .get_field::<serde_json::Value>("content")
            .ok()
            .flatten()
            .and_then(|content| content["body"].as_str().map(snippet))
            .unwrap_or_default(),
        Ok(Err(err)) => {
            tracing::debug!("thread root {root} fetch failed: {err}");
            String::new()
        }
        Err(_) => String::new(),
    };
    shared
        .thread_titles
        .lock()
        .expect("thread titles lock")
        .insert(root.to_owned(), title.clone());
    title
}

/// First line, at most 60 code points, ellipsis when shortened.
fn snippet(body: &str) -> String {
    let line = body.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(60).collect();
    if out.len() < line.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_id_codec_round_trips() {
        let room = RoomId::parse("!r:localhost").unwrap();
        let root = EventId::parse("$root").unwrap();
        let derived = thread_chat_id(&room, &root);
        assert_eq!(derived, "!r:localhost;thread=$root");
        assert_eq!(split_chat_id(&derived), ("!r:localhost", Some("$root")));
        assert_eq!(split_chat_id("!r:localhost"), ("!r:localhost", None));
    }

    #[test]
    fn snippets_take_the_first_line_bounded() {
        assert_eq!(snippet("short"), "short");
        assert_eq!(snippet("first line\nsecond"), "first line");
        let long = "x".repeat(80);
        let s = snippet(&long);
        assert_eq!(s.chars().count(), 61, "60 + ellipsis");
        assert!(s.ends_with('…'));
    }
}
