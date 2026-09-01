//! Inbound translation: Matrix timeline events → connproto frames, plus
//! invite handling.
//!
//! Handlers are installed AFTER the backlog-discarding initial sync (see
//! client.rs), so only live post-connect events ever reach the host.

use std::sync::Arc;
use std::time::{Duration, Instant};

use matrix_sdk::ruma::events::reaction::OriginalSyncReactionEvent;
use matrix_sdk::ruma::events::relation::Replacement;
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::member::{
    MembershipState, OriginalSyncRoomMemberEvent, StrippedRoomMemberEvent,
};
use matrix_sdk::ruma::events::room::message::sanitize::remove_plain_reply_fallback;
use matrix_sdk::ruma::events::room::message::{
    MessageFormat, MessageType, OriginalSyncRoomMessageEvent, Relation,
    RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::events::room::redaction::OriginalSyncRoomRedactionEvent;
use matrix_sdk::ruma::events::sticker::OriginalSyncStickerEvent;
use matrix_sdk::{Room, RoomState};

use crate::proto::{
    ChatMembershipFromConn, ConnFrame, MembershipChat, MessageDeletedFromConn,
    MessageEditedFromConn, MessageFromConn, ReactionFromConn,
};

use super::{asks, entities, media, rooms, Inviter, SeenReaction, Shared};

pub(crate) fn install_handlers(shared: Arc<Shared>) {
    let client = shared.client.clone();
    // Handlers must hold Weak: the registry lives inside the Client and
    // Shared holds that Client, so an Arc here would be a reference cycle —
    // the session (and its FrameSink) would never drop, and the connector
    // could not exit promptly on shutdown.
    let weak = Arc::downgrade(&shared);
    client.add_event_handler({
        let weak = weak.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room| {
            let weak = weak.clone();
            async move {
                if let Some(shared) = weak.upgrade() {
                    handle_message(shared, ev, room).await
                }
            }
        }
    });
    client.add_event_handler({
        let weak = weak.clone();
        move |ev: StrippedRoomMemberEvent, room: Room| {
            let weak = weak.clone();
            async move {
                if let Some(shared) = weak.upgrade() {
                    handle_invite(shared, ev, room).await
                }
            }
        }
    });
    // Fires ONLY for events that stayed encrypted after the decryption pass
    // (successfully decrypted ones dispatch as their real type) — i.e. UTDs.
    client.add_event_handler({
        let weak = weak.clone();
        move |ev: OriginalSyncRoomEncryptedEvent, room: Room| {
            let weak = weak.clone();
            async move {
                if let Some(shared) = weak.upgrade() {
                    handle_utd(shared, ev, room);
                }
            }
        }
    });
    // The bot's OWN membership → chat_membership frames (feature
    // chat_membership); fires for both timeline and state-section events.
    client.add_event_handler({
        let weak = weak.clone();
        move |ev: OriginalSyncRoomMemberEvent, room: Room| {
            let weak = weak.clone();
            async move {
                if let Some(shared) = weak.upgrade() {
                    handle_own_membership(shared, ev, room).await
                }
            }
        }
    });
    // Reactions in (feature reactions_in) — ask answers included.
    client.add_event_handler({
        let weak = weak.clone();
        move |ev: OriginalSyncReactionEvent, room: Room| {
            let weak = weak.clone();
            async move {
                if let Some(shared) = weak.upgrade() {
                    handle_reaction(shared, ev, room).await;
                }
            }
        }
    });
    // Redactions → message_deleted or reaction removed (features
    // deletes_in / reactions_in).
    client.add_event_handler({
        let weak = weak.clone();
        move |ev: OriginalSyncRoomRedactionEvent, room: Room| {
            let weak = weak.clone();
            async move {
                if let Some(shared) = weak.upgrade() {
                    handle_redaction(shared, ev, room);
                }
            }
        }
    });
    // Stickers are their own event type, not an m.room.message msgtype.
    client.add_event_handler({
        move |ev: OriginalSyncStickerEvent, room: Room| {
            let weak = weak.clone();
            async move {
                if let Some(shared) = weak.upgrade() {
                    media::handle_sticker(shared, ev, room).await
                }
            }
        }
    });
}

/// A reaction toggled on: an answer when it lands on an open ask's message
/// with an option emoji; otherwise deliver it and remember its event id so
/// a later redaction reads as `removed` — attributed to the reactor.
async fn handle_reaction(shared: Arc<Shared>, ev: OriginalSyncReactionEvent, room: Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    if Some(ev.sender.as_ref()) == shared.client.user_id() {
        tracing::debug!(event = %ev.event_id, "own reaction filtered (echo hygiene)");
        return;
    }
    if asks::try_answer(&shared, &ev, &room).await {
        return;
    }
    let annotation = &ev.content.relates_to;
    // The host correlates by (chat_id, id): a reaction to a thread-resident
    // message must carry the thread chat the message was delivered under.
    let chat_id = rooms::event_chat_id(&shared, &room, &annotation.event_id).await;
    let reaction = ReactionFromConn {
        chat_id,
        message_id: annotation.event_id.to_string(),
        user_id: ev.sender.to_string(),
        username: ev.sender.localpart().to_string(),
        key: annotation.key.clone(),
        removed: false,
    };
    shared
        .seen_reactions
        .lock()
        .expect("reactions lock")
        .insert(
            ev.event_id.clone(),
            SeenReaction {
                chat_id: reaction.chat_id.clone(),
                message_id: reaction.message_id.clone(),
                key: reaction.key.clone(),
                user_id: reaction.user_id.clone(),
                username: reaction.username.clone(),
            },
        );
    tracing::info!(
        event = %ev.event_id,
        target = %reaction.message_id,
        sender = %ev.sender,
        "reaction delivered"
    );
    shared.sink.send(ConnFrame::Reaction(reaction));
}

/// A redaction of a reaction we saw → `reaction removed`; anything else →
/// `message_deleted` (the host ignores ids it never met; deletions of the
/// bot's own messages are dropped host-side).
fn handle_redaction(shared: Arc<Shared>, ev: OriginalSyncRoomRedactionEvent, room: Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    if Some(ev.sender.as_ref()) == shared.client.user_id() {
        return; // our own deletes/reaction-removals never come back inbound
    }
    // Room v11 moved `redacts` into content; accept either location.
    let Some(redacts) = ev.redacts.clone().or_else(|| ev.content.redacts.clone()) else {
        return;
    };
    // An un-tapped ask answer is widget traffic, not a deletion.
    if shared
        .answer_events
        .lock()
        .expect("answer events lock")
        .remove(&redacts)
        .is_some()
    {
        return;
    }
    let seen = shared
        .seen_reactions
        .lock()
        .expect("reactions lock")
        .remove(&redacts);
    match seen {
        Some(seen) => {
            tracing::info!(redacted = %redacts, "reaction removal delivered");
            shared.sink.send(ConnFrame::Reaction(ReactionFromConn {
                chat_id: seen.chat_id,
                message_id: seen.message_id,
                user_id: seen.user_id,
                username: seen.username,
                key: seen.key,
                removed: true,
            }))
        }
        None => {
            tracing::info!(redacted = %redacts, "message_deleted delivered");
            shared
                .sink
                .send(ConnFrame::MessageDeleted(MessageDeletedFromConn {
                    chat_id: rooms::cached_event_chat_id(&shared, &room, &redacts),
                    id: redacts.to_string(),
                }))
        }
    }
}

/// The bot landed in (or fell out of) a chat — the host's group-admission
/// hook. Only our own membership is tracked, never other members' churn.
/// The announced set dedupes state re-delivery and keeps pre-session joins
/// silent (it is seeded with the joined set at connect).
async fn handle_own_membership(shared: Arc<Shared>, ev: OriginalSyncRoomMemberEvent, room: Room) {
    if Some(ev.state_key.as_ref()) != shared.client.user_id() {
        return;
    }
    let change = match ev.content.membership {
        MembershipState::Join => {
            let fresh = shared
                .announced
                .lock()
                .expect("announced lock")
                .insert(room.room_id().to_owned());
            if !fresh {
                return; // already announced (or joined before this session)
            }
            "added"
        }
        MembershipState::Leave | MembershipState::Ban => {
            let known = shared
                .announced
                .lock()
                .expect("announced lock")
                .remove(room.room_id());
            if !known {
                return; // never in: a rejected/retracted invite, not a removal
            }
            "removed"
        }
        _ => return,
    };
    let (by_user_id, by_username) = if change == "added" {
        shared
            .inviters
            .lock()
            .expect("inviters lock")
            .remove(room.room_id())
            .map(|inviter| (inviter.user_id, inviter.username))
            .unwrap_or_default()
    } else {
        // The leave/kick/ban event's sender is who did it (us, if we left).
        (ev.sender.to_string(), ev.sender.localpart().to_string())
    };
    let (kind, title) = rooms::chat_shape(&shared, &room).await;
    tracing::info!(
        room = %room.room_id(),
        change,
        by = %by_user_id,
        "chat_membership delivered"
    );
    shared
        .sink
        .send(ConnFrame::ChatMembership(ChatMembershipFromConn {
            chat: MembershipChat {
                id: room.room_id().to_string(),
                kind,
                title,
            },
            change: change.into(),
            by_user_id,
            by_username,
            ..Default::default()
        }));
}

/// Unable-to-decrypt: log every event, warn the operator once per burst.
/// The SDK retries via key backup downloads (AfterDecryptionFailure); a
/// persistent stream of these means the device needs verification/recovery.
fn handle_utd(shared: Arc<Shared>, ev: OriginalSyncRoomEncryptedEvent, room: Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    tracing::warn!(
        "cannot decrypt event {} in {} from {}",
        ev.event_id,
        room.room_id(),
        ev.sender
    );
    if shared
        .utd_warns
        .lock()
        .expect("utd limiter lock")
        .allow(Instant::now())
    {
        shared.sink.warn(format!(
            "cannot decrypt incoming messages in {} — the bot is missing room keys; \
             verify the terva device or restore recovery via `setup` (retrying via key backup)",
            room.room_id()
        ));
    }
}

/// One live timeline message → one `message` frame (or `message_edited`
/// for a replacement). Echo hygiene: our own events never go back inbound.
pub(crate) async fn handle_message(
    shared: Arc<Shared>,
    ev: OriginalSyncRoomMessageEvent,
    room: Room,
) {
    if room.state() != RoomState::Joined {
        return;
    }
    if Some(ev.sender.as_ref()) == shared.client.user_id() {
        tracing::debug!(event = %ev.event_id, "own message filtered (echo hygiene)");
        return; // never re-deliver the bot's own messages (or edits of them)
    }
    if let Some(Relation::Replacement(replacement)) = &ev.content.relates_to {
        return handle_edit(shared, &ev, replacement, room).await;
    }
    let MessageType::Text(text) = &ev.content.msgtype else {
        return media::handle_attachment_message(shared, ev, room).await;
    };
    let shape =
        rooms::inbound_shape(&shared, &room, ev.content.relates_to.as_ref(), &ev.event_id).await;
    // Real replies carry the quote fallback; thread fallbacks do not.
    let body = if shape.reply_to.is_empty() {
        text.body.as_str()
    } else {
        remove_plain_reply_fallback(&text.body)
    };
    let formatted_html = text
        .formatted
        .as_ref()
        .filter(|f| f.format == MessageFormat::Html)
        .map(|f| f.body.as_str());
    let replied_to_bot = !shape.reply_to.is_empty()
        && shared
            .sent_ids
            .lock()
            .expect("sent cache lock")
            .contains(&shape.reply_to);
    let entities = entities::extract(
        body,
        formatted_html,
        ev.content.mentions.as_ref(),
        replied_to_bot,
        &shared.identity,
    );
    tracing::info!(
        event = %ev.event_id,
        chat = %shape.chat_id,
        kind = %shape.chat_kind,
        sender = %ev.sender,
        text_len = body.chars().count(),
        entities = entities.len(),
        "message delivered"
    );
    shared.sink.send(ConnFrame::Message(MessageFromConn {
        id: ev.event_id.to_string(),
        ts: i64::from(ev.origin_server_ts.get()),
        chat_id: shape.chat_id,
        chat_kind: shape.chat_kind,
        chat_title: shape.chat_title,
        user_id: ev.sender.to_string(),
        username: ev.sender.localpart().to_string(),
        reply_to: shape.reply_to,
        text: body.to_string(),
        entities,
        ..Default::default()
    }));
}

/// An `m.replace` edit → `message_edited` under the ORIGINAL event id with
/// the replacement text (feature edits_in). The chain always points at the
/// original, so latest-wins collapsing is inherent. Only text→text edits
/// translate; attachment caption edits are noise the host can live without.
async fn handle_edit(
    shared: Arc<Shared>,
    ev: &OriginalSyncRoomMessageEvent,
    replacement: &Replacement<RoomMessageEventContentWithoutRelation>,
    room: Room,
) {
    let new_content = &replacement.new_content;
    let MessageType::Text(text) = &new_content.msgtype else {
        tracing::debug!(event = %ev.event_id, "non-text edit skipped");
        return;
    };
    // The spec says new_content carries no reply fallback; entities come
    // from the edit's mentions (new_content's, or the duplicate the spec
    // puts at the top level). A reply's original relation lives on the
    // original event, so reply-to-bot is not re-derivable here — m.mentions
    // covers it (clients re-include the replied-to sender).
    let formatted_html = text
        .formatted
        .as_ref()
        .filter(|f| f.format == MessageFormat::Html)
        .map(|f| f.body.as_str());
    let mentions = new_content
        .mentions
        .as_ref()
        .or(ev.content.mentions.as_ref());
    let entities = entities::extract(
        &text.body,
        formatted_html,
        mentions,
        false,
        &shared.identity,
    );
    tracing::info!(
        original = %replacement.event_id,
        edit = %ev.event_id,
        "message_edited delivered"
    );
    // Under the same chat id the ORIGINAL was delivered with — the edit
    // event never re-states its thread, but the host matches (chat_id, id).
    let chat_id = rooms::event_chat_id(&shared, &room, &replacement.event_id).await;
    shared
        .sink
        .send(ConnFrame::MessageEdited(MessageEditedFromConn {
            chat_id,
            id: replacement.event_id.to_string(),
            ts: i64::from(ev.origin_server_ts.get()),
            text: text.body.clone(),
            entities,
        }));
}

/// Auto-join invites addressed to us (config-gated). The `chat_membership
/// added` frame is emitted by `handle_own_membership` when the join lands;
/// the host's admission gate stays in charge either way.
async fn handle_invite(shared: Arc<Shared>, ev: StrippedRoomMemberEvent, room: Room) {
    if Some(ev.state_key.as_ref()) != shared.client.user_id() {
        return; // someone else's membership
    }
    if ev.content.membership != MembershipState::Invite {
        return;
    }
    // Recorded ahead of the auto_join gate: however the join happens (here,
    // or an operator from another client), the membership frame names them.
    shared.inviters.lock().expect("inviters lock").insert(
        room.room_id().to_owned(),
        Inviter {
            user_id: ev.sender.to_string(),
            username: ev.sender.localpart().to_string(),
        },
    );
    if !shared.config.auto_join_enabled() {
        tracing::info!("ignoring invite to {} (auto_join = never)", room.room_id());
        return;
    }
    // The invite's is_direct flag is the only DM signal we have before
    // m.direct account data catches up — remember it, then persist it.
    let direct = ev.content.is_direct.unwrap_or(false);
    if direct {
        shared
            .dm_rooms
            .lock()
            .expect("dm set lock")
            .insert(room.room_id().to_owned());
    }
    join_with_retry(&room).await;
    if direct && room.state() == RoomState::Joined {
        if let Err(err) = room.set_is_direct(true).await {
            tracing::warn!("marking {} as direct failed: {err}", room.room_id());
        }
    }
}

/// Reconcile invites that predate this session (they are state, not events —
/// the stripped-member handler never sees them).
pub(crate) async fn join_pending_invites(shared: Arc<Shared>) {
    if !shared.config.auto_join_enabled() {
        return;
    }
    for room in shared.client.invited_rooms() {
        tracing::info!("joining pending invite to {}", room.room_id());
        // Must read the inviter before joining — invite_details only answers
        // while the room is still in the Invited state.
        match room.invite_details().await {
            Ok(details) => {
                shared.inviters.lock().expect("inviters lock").insert(
                    room.room_id().to_owned(),
                    Inviter {
                        user_id: details.inviter_id.to_string(),
                        username: details.inviter_id.localpart().to_string(),
                    },
                );
            }
            Err(err) => {
                tracing::warn!("no invite details for {}: {err}", room.room_id());
            }
        }
        join_with_retry(&room).await;
    }
}

/// Homeservers commonly 500 a join right after the invite lands — retry a
/// few times before giving up.
async fn join_with_retry(room: &Room) {
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
        }
        match room.join().await {
            Ok(()) => {
                tracing::info!("joined {}", room.room_id());
                return;
            }
            Err(err) => tracing::warn!(
                "join {} failed (attempt {}): {err}",
                room.room_id(),
                attempt + 1
            ),
        }
    }
}
