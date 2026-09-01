//! Outbound command handlers: host frames → Matrix REST calls.

use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::room::{IncludeRelations, RelationsOptions};
use matrix_sdk::ruma::api::client::typing::create_typing_event::v3::{
    Request as TypingRequest, Typing, TypingInfo,
};
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::{Annotation, RelationType, Thread};
use matrix_sdk::ruma::events::room::message::{
    Relation, ReplacementMetadata, RoomMessageEventContent,
};
use matrix_sdk::ruma::events::TimelineEventType;
use matrix_sdk::ruma::{uint, EventId, OwnedEventId};
use matrix_sdk::Room;

use crate::proto::{DeleteFromHost, EditFromHost, ReactFromHost, SendFromHost};
use crate::serve::{CommandOk, Responder};

use super::{media, rooms, Shared, COMMAND_TIMEOUT};

/// `send` → markdown-rendered `m.room.message` (terva replies are
/// markdown-shaped and Matrix renders it properly). `reply_to` becomes a
/// rich-reply relation; a thread-derived chat id threads the message.
/// Chunking to `max_text_len` is host-side.
pub(crate) async fn send(shared: Arc<Shared>, cmd: SendFromHost, responder: Responder) {
    let target = match rooms::resolve_chat(&shared, &cmd.chat_id) {
        Ok(target) => target,
        Err(err) => return responder.err(err),
    };
    let mut content = RoomMessageEventContent::text_markdown(cmd.text.as_str());
    content.relates_to = rooms::outbound_relation(&shared, &target, &cmd.reply_to);
    let sent = match speaker_profile(&shared, &cmd).await {
        Some(profile) => {
            // MSC4144 per-message profile: same content plus the unstable
            // profile field (the name mautrix bridges emit and clients
            // render today). send_raw rides the ordinary send machinery,
            // encryption included.
            let mut value = serde_json::to_value(&content).expect("content serializes");
            value["com.beeper.per_message_profile"] = profile;
            tokio::time::timeout(
                COMMAND_TIMEOUT,
                target.room.send_raw("m.room.message", value),
            )
            .await
        }
        None => tokio::time::timeout(COMMAND_TIMEOUT, target.room.send(content)).await,
    };
    match sent {
        Err(_) => responder.err("send timed out"),
        Ok(Err(err)) => responder.err(format!("send failed: {err}")),
        Ok(Ok(sent)) => {
            let event_id = sent.response.event_id.to_string();
            // A future reply to this event counts as a bot mention.
            shared
                .sent_ids
                .lock()
                .expect("sent cache lock")
                .insert(event_id.clone());
            match &target.thread_root {
                Some(root) => rooms::note_thread_event(&shared, root, &sent.response.event_id),
                None => rooms::note_room_event(&shared, &sent.response.event_id),
            }
            responder.ok(CommandOk {
                message_id: event_id,
                ..Default::default()
            });
        }
    }
}

/// The MSC4144 profile for a cast send, when the config declares a
/// speaker grade (the host only sends `speaker` to declared connectors;
/// the config gate makes a stale host harmless). `speaker:full` uploads
/// the avatar once per (key, path); failures degrade to name-only —
/// cast rendering must never fail the send.
async fn speaker_profile(shared: &Shared, cmd: &SendFromHost) -> Option<serde_json::Value> {
    let speaker = cmd.speaker.as_ref()?;
    let grade = shared.config.speaker_feature()?;
    if speaker.name.is_empty() {
        return None;
    }
    let mut profile = serde_json::json!({
        "id": speaker.key,
        "displayname": speaker.name,
    });
    if grade == "speaker:full" && !speaker.avatar_path.is_empty() {
        if let Some(mxc) = avatar_uri(shared, &speaker.key, &speaker.avatar_path).await {
            profile["avatar_url"] = mxc.into();
        }
    }
    Some(profile)
}

/// Upload (or recall) a cast member's avatar. Cached by (key, path) so a
/// changed avatar file re-uploads while a stable cast uploads once.
async fn avatar_uri(shared: &Shared, key: &str, path: &str) -> Option<String> {
    let cache_key = (key.to_string(), path.to_string());
    if let Some(uri) = shared
        .avatar_uris
        .lock()
        .expect("avatar cache lock")
        .get(&cache_key)
        .cloned()
    {
        return Some(uri);
    }
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) => {
            tracing::warn!("cannot read speaker avatar {path}: {err}");
            return None;
        }
    };
    let mime = media::guess_mime(std::path::Path::new(path));
    let upload = shared.client.media().upload(&mime, data, None);
    match tokio::time::timeout(Duration::from_secs(10), upload).await {
        Ok(Ok(response)) => {
            let uri = response.content_uri.to_string();
            shared
                .avatar_uris
                .lock()
                .expect("avatar cache lock")
                .insert(cache_key, uri.clone());
            Some(uri)
        }
        Ok(Err(err)) => {
            tracing::warn!("speaker avatar upload failed for {key}: {err}");
            None
        }
        Err(_) => {
            tracing::warn!("speaker avatar upload timed out for {key}");
            None
        }
    }
}

/// `thread_start` → open a work-stream thread (feature threads_out). With
/// an anchor, the starter message (the name) threads off it; anchorless,
/// the starter itself becomes the root and later sends thread off IT. The
/// result's chat_id is the derived `<room_id>;thread=<root>` chat.
pub(crate) async fn thread_start(
    shared: Arc<Shared>,
    cmd: crate::proto::ThreadStartFromHost,
    responder: Responder,
) {
    let target = match rooms::resolve_chat(&shared, &cmd.chat_id) {
        Ok(target) => target,
        Err(err) => return responder.err(err),
    };
    if target.thread_root.is_some() {
        return responder.err("threads do not nest — thread_start needs a plain chat");
    }
    if cmd.name.is_empty() {
        return responder.err("thread_start carries no name");
    }
    let room = target.room;
    let anchor = if cmd.from_message_id.is_empty() {
        None
    } else {
        match EventId::parse(&cmd.from_message_id) {
            Ok(id) => Some(id.to_owned()),
            Err(err) => {
                return responder.err(format!(
                    "bad from_message_id {:?}: {err}",
                    cmd.from_message_id
                ))
            }
        }
    };
    let mut content = RoomMessageEventContent::text_markdown(cmd.name.as_str());
    if let Some(root) = &anchor {
        content.relates_to = Some(Relation::Thread(Thread::plain(root.clone(), root.clone())));
    }
    match tokio::time::timeout(COMMAND_TIMEOUT, room.send(content)).await {
        Err(_) => responder.err("thread starter send timed out"),
        Ok(Err(err)) => responder.err(format!("thread starter send failed: {err}")),
        Ok(Ok(sent)) => {
            let starter = sent.response.event_id.clone();
            let root = anchor.unwrap_or_else(|| starter.clone());
            rooms::note_thread_event(&shared, &root, &starter);
            shared
                .sent_ids
                .lock()
                .expect("sent cache lock")
                .insert(starter.to_string());
            responder.ok(CommandOk {
                message_id: starter.to_string(),
                chat_id: rooms::thread_chat_id(room.room_id(), &root),
            });
        }
    }
}

/// `edit` → an `m.replace` of `message_id` with markdown-rendered new
/// content (feature edits_out). The host keeps referencing the ORIGINAL id;
/// the fallback `* text` body is for clients that never learned edits.
pub(crate) async fn edit(shared: Arc<Shared>, cmd: EditFromHost, responder: Responder) {
    let room = match rooms::resolve_chat(&shared, &cmd.chat_id) {
        Ok(target) => target.room,
        Err(err) => return responder.err(err),
    };
    let target = match EventId::parse(&cmd.message_id) {
        Ok(id) => id,
        Err(err) => return responder.err(format!("bad message id {:?}: {err}", cmd.message_id)),
    };
    let content = RoomMessageEventContent::text_markdown(cmd.text.as_str())
        .make_replacement(ReplacementMetadata::new(target, None));
    match tokio::time::timeout(COMMAND_TIMEOUT, room.send(content)).await {
        Err(_) => responder.err("edit timed out"),
        Ok(Err(err)) => responder.err(format!("edit failed: {err}")),
        Ok(Ok(_)) => responder.ok(CommandOk::default()),
    }
}

/// `delete` → redact the message (feature deletes_out).
pub(crate) async fn delete(shared: Arc<Shared>, cmd: DeleteFromHost, responder: Responder) {
    let room = match rooms::resolve_chat(&shared, &cmd.chat_id) {
        Ok(target) => target.room,
        Err(err) => return responder.err(err),
    };
    let target = match EventId::parse(&cmd.message_id) {
        Ok(id) => id,
        Err(err) => return responder.err(format!("bad message id {:?}: {err}", cmd.message_id)),
    };
    match tokio::time::timeout(COMMAND_TIMEOUT, room.redact(&target, None, None)).await {
        Err(_) => responder.err("delete timed out"),
        Ok(Err(err)) => responder.err(format!("delete failed: {err}")),
        Ok(Ok(_)) => responder.ok(CommandOk::default()),
    }
}

/// `react` → an `m.annotation` reaction; `remove: true` → redact OUR
/// reaction for that key (feature reactions_out). The (message, key) → our
/// event id map makes removal a single call; entries lost to a restart or
/// eviction rebuild lazily from `/relations`.
pub(crate) async fn react(shared: Arc<Shared>, cmd: ReactFromHost, responder: Responder) {
    let room = match rooms::resolve_chat(&shared, &cmd.chat_id) {
        Ok(target) => target.room,
        Err(err) => return responder.err(err),
    };
    let target = match EventId::parse(&cmd.message_id) {
        Ok(id) => id,
        Err(err) => return responder.err(format!("bad message id {:?}: {err}", cmd.message_id)),
    };
    if !cmd.remove {
        let content = ReactionEventContent::new(Annotation::new(target, cmd.key.clone()));
        match tokio::time::timeout(COMMAND_TIMEOUT, room.send(content)).await {
            Err(_) => responder.err("react timed out"),
            Ok(Err(err)) => responder.err(format!("react failed: {err}")),
            Ok(Ok(sent)) => {
                shared
                    .our_reactions
                    .lock()
                    .expect("our reactions lock")
                    .insert(
                        (cmd.message_id.clone(), cmd.key.clone()),
                        sent.response.event_id.clone(),
                    );
                responder.ok(CommandOk::default());
            }
        }
        return;
    }
    let cached = shared
        .our_reactions
        .lock()
        .expect("our reactions lock")
        .remove(&(cmd.message_id.clone(), cmd.key.clone()));
    let ours = match cached {
        Some(event_id) => Some(event_id),
        None => find_our_reaction(&shared, &room, &target, &cmd.key).await,
    };
    let Some(ours) = ours else {
        return responder.err(format!(
            "no reaction of ours with key {:?} to remove",
            cmd.key
        ));
    };
    match tokio::time::timeout(COMMAND_TIMEOUT, room.redact(&ours, None, None)).await {
        Err(_) => responder.err("react remove timed out"),
        Ok(Err(err)) => responder.err(format!("react remove failed: {err}")),
        Ok(Ok(_)) => responder.ok(CommandOk::default()),
    }
}

/// Ask the server for the target's `m.annotation` relations and pick out
/// our own reaction with the requested key.
async fn find_our_reaction(
    shared: &Shared,
    room: &Room,
    target: &EventId,
    key: &str,
) -> Option<OwnedEventId> {
    let own = shared.client.user_id()?;
    let opts = RelationsOptions {
        include_relations: IncludeRelations::RelationsOfTypeAndEventType(
            RelationType::Annotation,
            TimelineEventType::Reaction,
        ),
        limit: Some(uint!(100)),
        ..Default::default()
    };
    let relations = match room.relations(target.to_owned(), opts).await {
        Ok(relations) => relations,
        Err(err) => {
            tracing::warn!("relations lookup for {target} failed: {err}");
            return None;
        }
    };
    for event in relations.chunk {
        let Ok(value) = event.raw().deserialize_as::<serde_json::Value>() else {
            continue;
        };
        if value["type"] == "m.reaction"
            && value["sender"] == own.as_str()
            && value["content"]["m.relates_to"]["key"] == key
        {
            if let Some(id) = value["event_id"].as_str() {
                return EventId::parse(id).ok().map(|id| id.to_owned());
            }
        }
    }
    None
}

/// Fire-and-forget typing indicator. Active, we PUT a 30 s server-side
/// timeout and declare `typing_refresh_ms: 20000`, so the host re-asserts
/// before expiry; inactive (the host's `typing_stop`, one frame after each
/// reply) we PUT `typing: false`, which clears the indicator at once
/// instead of letting the 30 s run out beside the answer. (The SDK's
/// `typing_notice` helper uses a 4 s timeout — useless at the host's
/// refresh cadence — hence the raw request.)
pub(crate) async fn typing(shared: Arc<Shared>, chat_id: String, active: bool) {
    // Matrix typing is room-scoped; a thread chat types in its room.
    let room = match rooms::resolve_chat(&shared, &chat_id) {
        Ok(target) => target.room,
        Err(err) => {
            tracing::warn!("typing dropped: {err}");
            return;
        }
    };
    let user_id = shared.client.user_id().expect("connected").to_owned();
    let state = if active {
        Typing::Yes(TypingInfo::new(Duration::from_secs(30)))
    } else {
        Typing::No
    };
    let request = TypingRequest::new(user_id, room.room_id().to_owned(), state);
    if let Err(err) = shared.client.send(request).await {
        let what = if active {
            "typing notice"
        } else {
            "typing stop"
        };
        tracing::warn!("{what} failed for {chat_id}: {err}");
    }
}
