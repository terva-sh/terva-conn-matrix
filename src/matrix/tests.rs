//! Matrix layer tests against `MatrixMockServer` (wiremock) — no homeserver
//! needed (PLAN.md §7.4).

use std::sync::{mpsc, Arc};
use std::time::Duration;

use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::{event_id, owned_user_id, room_id, user_id};
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk::Client;
use matrix_sdk_test::event_factory::EventFactory;
use matrix_sdk_test::JoinedRoomBuilder;

use crate::config::Config;
use crate::proto::{ConnFrame, SendFromHost};
use crate::serve::{FrameSink, Responder};

use super::{inbound, outbound, Inviter, Shared};

/// The detached FrameSink forwards through a bridge thread, so frames land
/// asynchronously: wait bounded for expected frames, and give stragglers a
/// beat before asserting absence.
const RECV: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(300);

fn test_shared(client: &Client) -> (Arc<Shared>, mpsc::Receiver<ConnFrame>) {
    let (frame_tx, frame_rx) = mpsc::channel();
    let (sink, _fatal_rx) = FrameSink::detached(frame_tx);
    let shared = Arc::new(Shared::new(
        client.clone(),
        Config {
            auto_join: "always".into(),
            max_attachment_mb: 64,
            ..Default::default()
        },
        sink,
        None,
        Some(test_data_dir()),
    ));
    (shared, frame_rx)
}

/// A per-process scratch data_dir (attachment ingest writes real files).
fn test_data_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("terva-conn-matrix-mock-{}", std::process::id()))
}

#[tokio::test]
async fn inbound_text_message_becomes_a_message_frame() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("hello agent")
                    .event_id(event_id!("$in1"))
                    .server_ts(1751469000123u64),
            ),
        )
        .await;

    let frame = frames.recv_timeout(RECV).expect("one message frame");
    let ConnFrame::Message(msg) = frame else {
        panic!("expected a message frame, got {frame:?}");
    };
    assert_eq!(msg.id, "$in1");
    assert_eq!(msg.ts, 1751469000123);
    assert_eq!(msg.chat_id, room_id.as_str());
    assert_eq!(msg.chat_kind, "group"); // no m.direct data → group
    assert_eq!(msg.user_id, "@drew:localhost");
    assert_eq!(msg.username, "drew");
    assert_eq!(msg.text, "hello agent");
    assert_eq!(msg.reply_to, "");
    assert!(frames.recv_timeout(QUIET).is_err(), "exactly one frame");
}

#[tokio::test]
async fn own_messages_are_never_redelivered_and_edits_translate() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;

    // Echo hygiene: the mock client's own user is @example:localhost.
    let own = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@example:localhost"));
    let other = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(own.text_msg("bot echo").event_id(event_id!("$own")))
                .add_timeline_event(
                    own.text_msg("* streamed")
                        .edit(
                            event_id!("$ours"),
                            matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain("streamed")
                                .into(),
                        )
                        .event_id(event_id!("$ownedit")),
                )
                .add_timeline_event(
                    other
                        .text_msg("* fixed")
                        .edit(
                            event_id!("$orig"),
                            matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain("fixed")
                                .into(),
                        )
                        .event_id(event_id!("$edit")),
                ),
        )
        .await;

    // The bot's own message and its own (streaming) edit stay silent; the
    // human's edit arrives as message_edited under the ORIGINAL id.
    let frame = frames.recv_timeout(RECV).expect("one frame");
    let ConnFrame::MessageEdited(edited) = frame else {
        panic!("expected message_edited, got {frame:?}");
    };
    assert_eq!(edited.id, "$orig");
    assert_eq!(edited.text, "fixed");
    assert_eq!(edited.chat_id, room_id.as_str());
    assert!(frames.recv_timeout(QUIET).is_err(), "exactly one frame");
}

#[tokio::test]
async fn replies_strip_the_fallback_and_carry_reply_to() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("> <@bot:localhost> earlier\n\nactual reply")
                    .reply_to(event_id!("$orig"))
                    .event_id(event_id!("$re1")),
            ),
        )
        .await;

    let ConnFrame::Message(msg) = frames.recv_timeout(RECV).expect("frame") else {
        panic!("expected message");
    };
    assert_eq!(msg.reply_to, "$orig");
    assert_eq!(msg.text, "actual reply", "fallback quote must be stripped");
}

#[tokio::test]
async fn dm_rooms_map_to_chat_kind_dm() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!dm:localhost");
    server.sync_joined_room(&client, room_id).await;
    // The is_direct invite flag was seen this session (the account-data path
    // is exercised live; this pins the session-local fallback).
    shared.dm_rooms.lock().unwrap().insert(room_id.to_owned());

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hi").event_id(event_id!("$dm1"))),
        )
        .await;

    let ConnFrame::Message(msg) = frames.recv_timeout(RECV).expect("frame") else {
        panic!("expected message");
    };
    assert_eq!(msg.chat_kind, "dm");
    assert_eq!(msg.chat_title, "", "DMs carry no title");
}

#[tokio::test]
async fn outbound_send_returns_the_event_id_and_renders_markdown() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "body": "**hi** drew",
            "format": "org.matrix.custom.html",
        }))
        .ok(event_id!("$out1"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::send(
        shared,
        SendFromHost {
            id: "s-1".into(),
            chat_id: room_id.to_string(),
            text: "**hi** drew".into(),
            ..Default::default()
        },
        Responder::detached("s-1", result_sink),
    )
    .await;

    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result frame") else {
        panic!("expected result");
    };
    assert_eq!(result.id, "s-1");
    assert_eq!(result.message_id, "$out1");
    assert_eq!(result.error, "");
    assert!(frames.recv_timeout(QUIET).is_err(), "no other frames");
}

#[tokio::test]
async fn outbound_reply_carries_the_rich_reply_relation() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.relates_to": { "m.in_reply_to": { "event_id": "$orig" } },
        }))
        .ok(event_id!("$out2"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::send(
        shared,
        SendFromHost {
            id: "s-2".into(),
            chat_id: room_id.to_string(),
            reply_to: "$orig".into(),
            text: "replying".into(),
            ..Default::default()
        },
        Responder::detached("s-2", result_sink),
    )
    .await;

    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result frame") else {
        panic!("expected result");
    };
    assert_eq!(result.message_id, "$out2");
    assert_eq!(result.error, "");
}

#[tokio::test]
async fn send_to_an_unknown_chat_fails_cleanly() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::send(
        shared,
        SendFromHost {
            id: "s-3".into(),
            chat_id: "!nowhere:localhost".into(),
            text: "hi".into(),
            ..Default::default()
        },
        Responder::detached("s-3", result_sink),
    )
    .await;

    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result frame") else {
        panic!("expected result");
    };
    assert_eq!(result.id, "s-3");
    assert!(
        result.error.contains("no such chat"),
        "got: {}",
        result.error
    );
}

/// Until the owner's first DM of a host run, terva addresses
/// owner-directed frames (admission asks, approvals, the idle nudge) by
/// the paired USER id (connproto-proposals §12). A user-id chat id
/// resolves to the existing `m.direct` DM room with that user.
#[tokio::test]
async fn user_id_chat_ids_resolve_to_the_dm_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!dm:localhost");
    let owner = user_id!("@drew:localhost");
    server.sync_joined_room(&client, room_id).await;
    // m.direct account data marks !dm:localhost as the DM with @drew.
    let f = EventFactory::new().sender(user_id!("@example:localhost"));
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.direct().add_user(owner.to_owned().into(), room_id));
        })
        .await;

    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .ok(event_id!("$cold1"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::send(
        shared,
        SendFromHost {
            id: "s-4".into(),
            chat_id: owner.to_string(),
            text: "may I join `ops`?".into(),
            ..Default::default()
        },
        Responder::detached("s-4", result_sink),
    )
    .await;

    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result frame") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "", "resolved to the DM and sent");
    assert_eq!(result.message_id, "$cold1");
}

#[tokio::test]
async fn user_id_chat_ids_without_a_dm_fail_with_a_pointer() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::send(
        shared,
        SendFromHost {
            id: "s-5".into(),
            chat_id: "@stranger:localhost".into(),
            text: "hello?".into(),
            ..Default::default()
        },
        Responder::detached("s-5", result_sink),
    )
    .await;

    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result frame") else {
        panic!("expected result");
    };
    assert!(
        result.error.contains("user id") && result.error.contains("§12"),
        "the error must diagnose the mis-addressing; got: {}",
        result.error
    );
}

/// Drain until the next `message` frame (membership frames may interleave).
fn next_message(frames: &mpsc::Receiver<ConnFrame>) -> crate::proto::MessageFromConn {
    let mut skipped = Vec::new();
    loop {
        match frames.recv_timeout(RECV) {
            Ok(ConnFrame::Message(msg)) => return msg,
            Ok(other) => skipped.push(other),
            Err(err) => panic!("no message frame ({err}); skipped: {skipped:?}"),
        }
    }
}

#[tokio::test]
async fn m_mentions_become_a_bot_mention_entity() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!group:localhost");
    server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("hey @example deploy it")
                    .mentions(Mentions::with_user_ids([owned_user_id!(
                        "@example:localhost"
                    )]))
                    .event_id(event_id!("$m1")),
            ),
        )
        .await;

    let msg = next_message(&frames);
    assert_eq!(msg.entities.len(), 1, "entities: {:?}", msg.entities);
    assert_eq!(msg.entities[0].kind, "bot_mention");
    assert_eq!(msg.entities[0].offset, 4);
    assert_eq!(msg.entities[0].length, 8); // "@example" in code points
}

#[tokio::test]
async fn html_pills_become_entities() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!group:localhost");
    server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_html(
                    "example and drew look",
                    concat!(
                        r#"<a href="https://matrix.to/#/@example:localhost">example</a> and "#,
                        r#"<a href="https://matrix.to/#/@drew:localhost">drew</a> look"#,
                    ),
                )
                .event_id(event_id!("$m2")),
            ),
        )
        .await;

    let msg = next_message(&frames);
    assert_eq!(msg.entities.len(), 2, "entities: {:?}", msg.entities);
    assert_eq!(msg.entities[0].kind, "bot_mention");
    assert_eq!((msg.entities[0].offset, msg.entities[0].length), (0, 7));
    assert_eq!(msg.entities[1].kind, "mention");
    assert_eq!(msg.entities[1].user_id, "@drew:localhost");
    assert_eq!((msg.entities[1].offset, msg.entities[1].length), (12, 4));
}

#[tokio::test]
async fn a_reply_to_our_own_message_is_a_bot_mention() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!group:localhost");
    server.sync_joined_room(&client, room_id).await;
    // As if outbound::send had returned $ours earlier this session.
    shared.sent_ids.lock().unwrap().insert("$ours".into());

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("sounds right")
                    .reply_to(event_id!("$ours"))
                    .event_id(event_id!("$m3")),
            ),
        )
        .await;

    let msg = next_message(&frames);
    assert_eq!(msg.reply_to, "$ours");
    assert_eq!(msg.entities.len(), 1, "entities: {:?}", msg.entities);
    assert_eq!(msg.entities[0].kind, "bot_mention");
    assert_eq!(
        (msg.entities[0].offset, msg.entities[0].length),
        (0, 0),
        "reply mention is present-but-unlocatable"
    );
}

#[tokio::test]
async fn own_join_emits_chat_membership_added_once() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    // Built before the room joins, so the announced set does not contain it.
    let (shared, frames) = test_shared(&client);
    shared.inviters.lock().unwrap().insert(
        room_id!("!ops:localhost").to_owned(),
        Inviter {
            user_id: "@drew:localhost".into(),
            username: "drew".into(),
        },
    );
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!ops:localhost");
    server.sync_joined_room(&client, room_id).await;

    let own_join = || {
        EventFactory::new()
            .room(room_id)
            .member(user_id!("@example:localhost"))
            .membership(MembershipState::Join)
    };
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(own_join().event_id(event_id!("$j1"))),
        )
        .await;

    let frame = frames.recv_timeout(RECV).expect("membership frame");
    let ConnFrame::ChatMembership(m) = frame else {
        panic!("expected chat_membership, got {frame:?}");
    };
    assert_eq!(m.chat.id, room_id.as_str());
    assert_eq!(m.chat.kind, "group");
    assert_eq!(m.change, "added");
    assert_eq!(m.by_user_id, "@drew:localhost");
    assert_eq!(m.by_username, "drew");

    // Re-delivered join state must not re-announce.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(own_join().event_id(event_id!("$j2"))),
        )
        .await;
    assert!(
        frames.recv_timeout(QUIET).is_err(),
        "exactly one added frame"
    );
}

#[tokio::test]
async fn own_kick_emits_chat_membership_removed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!ops:localhost");

    // Join the room BEFORE constructing Shared: a pre-session membership,
    // seeded into the announced set — its join is never re-announced, but
    // its removal is real news.
    server.sync_joined_room(&client, room_id).await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let kick = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@admin:localhost"))
        .member(user_id!("@example:localhost"))
        .membership(MembershipState::Leave);
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(kick.event_id(event_id!("$k1"))),
        )
        .await;

    let frame = frames.recv_timeout(RECV).expect("membership frame");
    let ConnFrame::ChatMembership(m) = frame else {
        panic!("expected chat_membership, got {frame:?}");
    };
    assert_eq!(m.chat.id, room_id.as_str());
    assert_eq!(m.change, "removed");
    assert_eq!(m.by_user_id, "@admin:localhost", "the kicker is attributed");
    assert_eq!(m.by_username, "admin");
    assert!(frames.recv_timeout(QUIET).is_err(), "exactly one frame");
}

#[tokio::test]
async fn reactions_translate_and_their_redaction_reads_as_removed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.reaction(event_id!("$target"), "👍")
                    .event_id(event_id!("$r1")),
            ),
        )
        .await;

    let frame = frames.recv_timeout(RECV).expect("reaction frame");
    let ConnFrame::Reaction(reaction) = frame else {
        panic!("expected reaction, got {frame:?}");
    };
    assert_eq!(reaction.message_id, "$target");
    assert_eq!(reaction.key, "👍");
    assert_eq!(reaction.user_id, "@drew:localhost");
    assert!(!reaction.removed);

    // Redacting the reaction event reads as removal, attributed to the
    // REACTOR (the moderator who redacted it is not who un-reacted).
    let moderator = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@admin:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                moderator
                    .redaction(event_id!("$r1"))
                    .event_id(event_id!("$rd1")),
            ),
        )
        .await;

    let frame = frames.recv_timeout(RECV).expect("removed frame");
    let ConnFrame::Reaction(removed) = frame else {
        panic!("expected reaction removal, got {frame:?}");
    };
    assert!(removed.removed);
    assert_eq!(removed.key, "👍");
    assert_eq!(removed.message_id, "$target");
    assert_eq!(
        removed.user_id, "@drew:localhost",
        "attributed to the reactor"
    );
    assert!(frames.recv_timeout(QUIET).is_err(), "no extra frames");
}

#[tokio::test]
async fn message_redaction_becomes_message_deleted() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redaction(event_id!("$msg")).event_id(event_id!("$rd2"))),
        )
        .await;

    let frame = frames.recv_timeout(RECV).expect("deleted frame");
    let ConnFrame::MessageDeleted(deleted) = frame else {
        panic!("expected message_deleted, got {frame:?}");
    };
    assert_eq!(deleted.id, "$msg");
    assert_eq!(deleted.chat_id, room_id.as_str());
}

#[tokio::test]
async fn own_reactions_and_redactions_stay_silent() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;

    let own = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@example:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    own.reaction(event_id!("$t"), "👀")
                        .event_id(event_id!("$or1")),
                )
                .add_timeline_event(own.redaction(event_id!("$x")).event_id(event_id!("$ord1"))),
        )
        .await;

    assert!(
        frames.recv_timeout(QUIET).is_err(),
        "the bot's own toggles and deletes must not come back inbound"
    );
}

#[tokio::test]
async fn outbound_edit_sends_a_replacement() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.new_content": { "body": "fixed text" },
            "m.relates_to": { "rel_type": "m.replace", "event_id": "$orig" },
        }))
        .ok(event_id!("$edit1"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::edit(
        shared,
        crate::proto::EditFromHost {
            id: "e-1".into(),
            chat_id: room_id.to_string(),
            message_id: "$orig".into(),
            text: "fixed text".into(),
        },
        Responder::detached("e-1", result_sink),
    )
    .await;

    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result") else {
        panic!("expected result");
    };
    assert_eq!(result.id, "e-1");
    assert_eq!(result.error, "");
}

#[tokio::test]
async fn outbound_delete_redacts_the_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server
        .mock_room_redact()
        .ok(event_id!("$rd"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::delete(
        shared,
        crate::proto::DeleteFromHost {
            id: "d-1".into(),
            chat_id: room_id.to_string(),
            message_id: "$gone".into(),
        },
        Responder::detached("d-1", result_sink),
    )
    .await;

    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result") else {
        panic!("expected result");
    };
    assert_eq!(result.id, "d-1");
    assert_eq!(result.error, "");
}

#[tokio::test]
async fn outbound_react_records_and_cached_remove_redacts() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.relates_to": { "rel_type": "m.annotation", "event_id": "$t", "key": "👀" },
        }))
        .ok(event_id!("$react1"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::react(
        shared.clone(),
        crate::proto::ReactFromHost {
            id: "r-1".into(),
            chat_id: room_id.to_string(),
            message_id: "$t".into(),
            key: "👀".into(),
            remove: false,
        },
        Responder::detached("r-1", result_sink.clone()),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("add result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");

    // Removal must use the remembered reaction event id — no /relations
    // round trip (that endpoint is not mocked, so a lookup would fail).
    server
        .mock_room_redact()
        .ok(event_id!("$rd3"))
        .mock_once()
        .mount()
        .await;
    outbound::react(
        shared,
        crate::proto::ReactFromHost {
            id: "r-2".into(),
            chat_id: room_id.to_string(),
            message_id: "$t".into(),
            key: "👀".into(),
            remove: true,
        },
        Responder::detached("r-2", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("remove result") else {
        panic!("expected result");
    };
    assert_eq!(result.id, "r-2");
    assert_eq!(result.error, "");
}

#[tokio::test]
async fn attachment_message_ingests_into_data_dir() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    // The SDK picks authenticated or legacy media by server capability —
    // mount both; each serves the same fake jpeg bytes.
    server.mock_media_download().ok_image().mount().await;
    server.mock_authed_media_download().ok_image().mount().await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.image(
                    "photo.jpg".to_owned(),
                    matrix_sdk::ruma::mxc_uri!("mxc://localhost/img1").to_owned(),
                )
                .event_id(event_id!("$img1")),
            ),
        )
        .await;

    let msg = next_message(&frames);
    assert_eq!(msg.id, "$img1");
    assert_eq!(msg.text, "", "caption-less image carries no text");
    assert_eq!(msg.attachments.len(), 1, "{:?}", msg.attachments);
    let att = &msg.attachments[0];
    assert_eq!(att.kind, "image");
    assert_eq!(att.name, "photo.jpg");
    assert_eq!(att.mime_type, "image/jpeg", "guessed from the extension");
    assert_eq!(att.caption, "");
    let bytes = std::fs::read(&att.path).expect("ingested file exists");
    assert_eq!(bytes, b"binaryjpegfullimagedata");
    assert_eq!(att.size, bytes.len() as i64);
    assert!(
        att.path.starts_with(test_data_dir().to_str().unwrap()),
        "attachment must land under data_dir: {}",
        att.path
    );
    let _ = std::fs::remove_file(&att.path);
}

/// Mount the mocks an ask needs and drive `handle_ask` to completion:
/// the question message (exact rendered body) plus one seed reaction per
/// option emoji. Returns the frames receiver for the follow-up assertions.
async fn open_test_ask(
    server: &MatrixMockServer,
    shared: &Arc<Shared>,
    restrict_to: Vec<String>,
) -> mpsc::Receiver<ConnFrame> {
    let room_id = room_id!("!room:localhost");
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "body": "Deploy to prod?\n\n- 👍 Approve\n- ① Deny\n",
        }))
        .ok(event_id!("$ask1"))
        .mock_once()
        .mount()
        .await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.relates_to": { "rel_type": "m.annotation", "event_id": "$ask1", "key": "👍" },
        }))
        .ok(event_id!("$seed1"))
        .mock_once()
        .mount()
        .await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.relates_to": { "rel_type": "m.annotation", "event_id": "$ask1", "key": "①" },
        }))
        .ok(event_id!("$seed2"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    super::asks::handle_ask(
        shared.clone(),
        crate::proto::AskFromHost {
            id: "a1".into(),
            chat_id: room_id.to_string(),
            text: "Deploy to prod?".into(),
            options: vec![
                crate::proto::AskOption {
                    key: "approve".into(),
                    label: "Approve".into(),
                    hint: "👍".into(),
                    ..Default::default()
                },
                crate::proto::AskOption {
                    key: "deny".into(),
                    label: "Deny".into(),
                    ..Default::default()
                },
            ],
            restrict_to,
            ..Default::default()
        },
        Responder::detached("a1", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("ask result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");
    assert_eq!(
        result.message_id, "$ask1",
        "result acknowledges the rendering"
    );
    result_rx
}

#[tokio::test]
async fn ask_translates_taps_into_attested_answers() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());
    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    // The imposter tap triggers a best-effort redact; let it succeed.
    server.mock_room_redact().ok(event_id!("$rd")).mount().await;
    let _rx = open_test_ask(&server, &shared, vec!["@drew:localhost".into()]).await;

    // An allowed tap — with a client-added variation selector — answers.
    let drew = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                drew.reaction(event_id!("$ask1"), "👍\u{FE0F}")
                    .event_id(event_id!("$tap1")),
            ),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("answer frame");
    let ConnFrame::Answer(answer) = frame else {
        panic!("expected answer, got {frame:?}");
    };
    assert_eq!(answer.ask_id, "a1");
    assert_eq!(answer.key, "approve");
    assert_eq!(answer.user_id, "@drew:localhost");
    assert_eq!(answer.attestation, "attested");

    // Un-tapping the answer is widget traffic — no frame.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                drew.redaction(event_id!("$tap1"))
                    .event_id(event_id!("$untap")),
            ),
        )
        .await;

    // A tap from outside restrict_to is filtered (and best-effort
    // redacted), never answered.
    let evil = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@evil:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                evil.reaction(event_id!("$ask1"), "👍")
                    .event_id(event_id!("$tap2")),
            ),
        )
        .await;

    // An unrelated emoji on the ask message is NOT an answer — it falls
    // through as a plain reaction for the host to note.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                drew.reaction(event_id!("$ask1"), "🎉")
                    .event_id(event_id!("$tap3")),
            ),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("plain reaction frame");
    let ConnFrame::Reaction(reaction) = frame else {
        panic!("expected plain reaction, got {frame:?}");
    };
    assert_eq!(reaction.key, "🎉");
    assert!(
        frames.recv_timeout(QUIET).is_err(),
        "no frame for the imposter"
    );
}

#[tokio::test]
async fn ask_close_withdraws_seeds_and_renders_the_outcome() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());
    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    let _rx = open_test_ask(&server, &shared, vec![]).await;

    server
        .mock_room_redact()
        .ok(event_id!("$rd"))
        .expect(2) // both seed reactions withdrawn
        .mount()
        .await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.new_content": { "body": "Deploy to prod?\n\n**Approved — drew**" },
            "m.relates_to": { "rel_type": "m.replace", "event_id": "$ask1" },
        }))
        .ok(event_id!("$closed"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    super::asks::handle_ask_close(
        shared.clone(),
        crate::proto::AskCloseFromHost {
            id: "c1".into(),
            ask_id: "a1".into(),
            outcome: "Approved — drew".into(),
        },
        Responder::detached("c1", result_sink.clone()),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("close result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");

    // The widget is gone: a late tap is a plain reaction again.
    let drew = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                drew.reaction(event_id!("$ask1"), "👍")
                    .event_id(event_id!("$late")),
            ),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("plain reaction");
    assert!(matches!(frame, ConnFrame::Reaction(_)), "got {frame:?}");

    // Closing twice (or an unknown ask) is an honest error.
    super::asks::handle_ask_close(
        shared,
        crate::proto::AskCloseFromHost {
            id: "c2".into(),
            ask_id: "a1".into(),
            outcome: String::new(),
        },
        Responder::detached("c2", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("second close") else {
        panic!("expected result");
    };
    assert!(
        result.error.contains("unknown ask"),
        "got: {}",
        result.error
    );
}

#[tokio::test]
async fn expired_asks_stop_translating_but_still_close() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());
    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_redact().ok(event_id!("$rd")).mount().await;
    let _rx = open_test_ask(&server, &shared, vec![]).await;

    super::asks::expire(shared.clone(), "a1".into()).await;

    // Post-expiry taps are plain reactions, not answers.
    let drew = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                drew.reaction(event_id!("$ask1"), "👍")
                    .event_id(event_id!("$late2")),
            ),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("plain reaction");
    assert!(matches!(frame, ConnFrame::Reaction(_)), "got {frame:?}");

    // A late close still renders the outcome (seeds already gone).
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.relates_to": { "rel_type": "m.replace", "event_id": "$ask1" },
        }))
        .ok(event_id!("$closed2"))
        .mock_once()
        .mount()
        .await;
    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    super::asks::handle_ask_close(
        shared,
        crate::proto::AskCloseFromHost {
            id: "c3".into(),
            ask_id: "a1".into(),
            outcome: "Timed out".into(),
        },
        Responder::detached("c3", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("close result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");
}

#[tokio::test]
async fn thread_start_anchored_returns_the_derived_chat() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "body": "investigate flakes",
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": "$anchor",
                "is_falling_back": true,
            },
        }))
        .ok(event_id!("$starter"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::thread_start(
        shared,
        crate::proto::ThreadStartFromHost {
            id: "t-1".into(),
            chat_id: room_id.to_string(),
            from_message_id: "$anchor".into(),
            name: "investigate flakes".into(),
        },
        Responder::detached("t-1", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");
    assert_eq!(result.chat_id, "!room:localhost;thread=$anchor");
    assert_eq!(result.message_id, "$starter");
}

#[tokio::test]
async fn thread_start_anchorless_roots_at_the_starter() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({ "body": "ops review" }))
        .ok(event_id!("$starter2"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::thread_start(
        shared,
        crate::proto::ThreadStartFromHost {
            id: "t-2".into(),
            chat_id: room_id.to_string(),
            from_message_id: String::new(),
            name: "ops review".into(),
        },
        Responder::detached("t-2", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");
    assert_eq!(
        result.chat_id, "!room:localhost;thread=$starter2",
        "anchorless threads root at their own starter"
    );
}

#[tokio::test]
async fn sends_into_a_thread_chat_carry_the_thread_relation() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    // No prior thread traffic: the fallback points at the root itself.
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": "$root",
                "m.in_reply_to": { "event_id": "$root" },
                "is_falling_back": true,
            },
        }))
        .ok(event_id!("$in-thread"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::send(
        shared,
        SendFromHost {
            id: "t-3".into(),
            chat_id: "!room:localhost;thread=$root".into(),
            text: "inside the thread".into(),
            ..Default::default()
        },
        Responder::detached("t-3", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");
    assert_eq!(result.message_id, "$in-thread");
}

#[tokio::test]
async fn inbound_thread_messages_route_to_the_derived_chat() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    // The root event fetch behind the title snippet.
    let root_event = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"))
        .text_msg("planning the refactor\nsecond line")
        .event_id(event_id!("$root"))
        .into_event();
    server.mock_room_event().ok(root_event).mount().await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("thread talk")
                    .in_thread(event_id!("$root"), event_id!("$prev"))
                    .event_id(event_id!("$t1")),
            ),
        )
        .await;

    let msg = next_message(&frames);
    assert_eq!(msg.chat_id, "!room:localhost;thread=$root");
    assert_eq!(msg.chat_kind, "thread");
    assert_eq!(msg.chat_title, "planning the refactor", "root snippet");
    assert_eq!(msg.text, "thread talk");
    assert_eq!(
        msg.reply_to, "",
        "a fallback in_reply_to is not a real reply"
    );

    // An explicit in-thread reply keeps its reply_to.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("agreed")
                    .in_thread_reply(event_id!("$root"), event_id!("$t1"))
                    .event_id(event_id!("$t2")),
            ),
        )
        .await;
    let msg = next_message(&frames);
    assert_eq!(msg.chat_id, "!room:localhost;thread=$root");
    assert_eq!(msg.reply_to, "$t1");
}

/// The host correlates message events on (chat_id, id) — an edit, delete,
/// or reaction touching a thread-resident message must carry the SAME
/// derived chat id its target was delivered under, not the room's.
#[tokio::test]
async fn thread_message_events_carry_the_delivered_chat_id() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    // Serves the title snippet fetch; the scope answers from the cache.
    let root_event = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"))
        .text_msg("planning the refactor")
        .event_id(event_id!("$root"))
        .into_event();
    server.mock_room_event().ok(root_event).mount().await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("thread talk")
                    .in_thread(event_id!("$root"), event_id!("$root"))
                    .event_id(event_id!("$t1")),
            ),
        )
        .await;
    let msg = next_message(&frames);
    let thread_chat = "!room:localhost;thread=$root";
    assert_eq!(msg.chat_id, thread_chat);

    // The edit event doesn't re-state its thread; the frame still must.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("* thread talk, corrected")
                    .edit(
                        event_id!("$t1"),
                        matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
                            "thread talk, corrected",
                        )
                        .into(),
                    )
                    .event_id(event_id!("$e1")),
            ),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("edited frame");
    let ConnFrame::MessageEdited(edited) = frame else {
        panic!("expected message_edited, got {frame:?}");
    };
    assert_eq!(edited.id, "$t1");
    assert_eq!(edited.chat_id, thread_chat, "edit rides the thread chat");

    // A reaction to the thread message, then its un-tap.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.reaction(event_id!("$t1"), "👍")
                    .event_id(event_id!("$r1")),
            ),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("reaction frame");
    let ConnFrame::Reaction(reaction) = frame else {
        panic!("expected reaction, got {frame:?}");
    };
    assert_eq!(reaction.message_id, "$t1");
    assert_eq!(
        reaction.chat_id, thread_chat,
        "reaction rides the thread chat"
    );

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redaction(event_id!("$r1")).event_id(event_id!("$rd1"))),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("removal frame");
    let ConnFrame::Reaction(removed) = frame else {
        panic!("expected reaction removal, got {frame:?}");
    };
    assert!(removed.removed);
    assert_eq!(
        removed.chat_id, thread_chat,
        "removal rides the thread chat"
    );

    // Deleting the thread message — the sharp case: a room-scoped frame
    // here would leave the host running a turn on a deleted message.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redaction(event_id!("$t1")).event_id(event_id!("$rd2"))),
        )
        .await;
    let frame = frames.recv_timeout(RECV).expect("deleted frame");
    let ConnFrame::MessageDeleted(deleted) = frame else {
        panic!("expected message_deleted, got {frame:?}");
    };
    assert_eq!(deleted.id, "$t1");
    assert_eq!(deleted.chat_id, thread_chat, "delete rides the thread chat");
    assert!(frames.recv_timeout(QUIET).is_err(), "no extra frames");
}

/// An edit of a message we never delivered this session (sent before a
/// restart, or evicted) resolves its scope by fetching the target and
/// reading its thread relation.
#[tokio::test]
async fn unseen_thread_targets_resolve_scope_by_fetching() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    // The fetched original carries its m.thread relation.
    let original = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"))
        .text_msg("pre-restart thread message")
        .in_thread(event_id!("$root"), event_id!("$root"))
        .event_id(event_id!("$old"))
        .into_event();
    server.mock_room_event().ok(original).mount().await;

    let f = EventFactory::new()
        .room(room_id)
        .sender(user_id!("@drew:localhost"));
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("* fixed")
                    .edit(
                        event_id!("$old"),
                        matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
                            "fixed",
                        )
                        .into(),
                    )
                    .event_id(event_id!("$e2")),
            ),
        )
        .await;

    let frame = frames.recv_timeout(RECV).expect("edited frame");
    let ConnFrame::MessageEdited(edited) = frame else {
        panic!("expected message_edited, got {frame:?}");
    };
    assert_eq!(edited.id, "$old");
    assert_eq!(
        edited.chat_id, "!room:localhost;thread=$root",
        "scope recovered from the fetched original"
    );
}

/// Shared with a specific speaker grade configured.
fn speaker_shared(client: &Client, grade: &str) -> (Arc<Shared>, mpsc::Receiver<ConnFrame>) {
    let (frame_tx, frame_rx) = mpsc::channel();
    let (sink, _fatal_rx) = FrameSink::detached(frame_tx);
    let shared = Arc::new(Shared::new(
        client.clone(),
        Config {
            auto_join: "always".into(),
            max_attachment_mb: 64,
            speaker: grade.into(),
            ..Default::default()
        },
        sink,
        None,
        Some(test_data_dir()),
    ));
    (shared, frame_rx)
}

#[tokio::test]
async fn speaker_sends_render_a_per_message_profile() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = speaker_shared(&client, "name_only");

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "body": "The airlock hisses open.",
            "com.beeper.per_message_profile": {
                "id": "kaiku",
                "displayname": "Kaiku",
            },
        }))
        .ok(event_id!("$cast1"))
        .mock_once()
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    outbound::send(
        shared,
        SendFromHost {
            id: "sp-1".into(),
            chat_id: room_id.to_string(),
            text: "The airlock hisses open.".into(),
            speaker: Some(crate::proto::Speaker {
                key: "kaiku".into(),
                name: "Kaiku".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        Responder::detached("sp-1", result_sink),
    )
    .await;
    let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result") else {
        panic!("expected result");
    };
    assert_eq!(result.error, "");
    assert_eq!(result.message_id, "$cast1");
}

#[tokio::test]
async fn speaker_full_uploads_the_avatar_once() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, _frames) = speaker_shared(&client, "full");

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    server.mock_room_state_encryption().plain().mount().await;

    let avatar = test_data_dir().join("kaiku-avatar.png");
    std::fs::create_dir_all(test_data_dir()).unwrap();
    std::fs::write(&avatar, b"pngbytes").unwrap();

    server
        .mock_authenticated_media_config()
        .ok(matrix_sdk::ruma::uint!(67108864))
        .mount()
        .await;
    server
        .mock_upload()
        .expect_mime_type("image/png")
        .ok(matrix_sdk::ruma::mxc_uri!("mxc://localhost/av1"))
        .mock_once() // the second send must hit the cache, not the server
        .mount()
        .await;
    server
        .mock_room_send()
        .body_matches_partial_json(serde_json::json!({
            "com.beeper.per_message_profile": {
                "id": "kaiku",
                "displayname": "Kaiku",
                "avatar_url": "mxc://localhost/av1",
            },
        }))
        .ok(event_id!("$cast2"))
        .expect(2)
        .mount()
        .await;

    let (result_tx, result_rx) = mpsc::channel();
    let (result_sink, _fatal) = FrameSink::detached(result_tx);
    for id in ["sp-2", "sp-3"] {
        outbound::send(
            shared.clone(),
            SendFromHost {
                id: id.into(),
                chat_id: room_id.to_string(),
                text: "line".into(),
                speaker: Some(crate::proto::Speaker {
                    key: "kaiku".into(),
                    name: "Kaiku".into(),
                    avatar_path: avatar.to_str().unwrap().into(),
                }),
                ..Default::default()
            },
            Responder::detached(id, result_sink.clone()),
        )
        .await;
        let ConnFrame::Result(result) = result_rx.recv_timeout(RECV).expect("result") else {
            panic!("expected result");
        };
        assert_eq!(result.error, "");
    }
    let _ = std::fs::remove_file(&avatar);
}

#[test]
fn speaker_grades_gate_on_config() {
    let feature = |s: &str| {
        Config {
            speaker: s.into(),
            ..Default::default()
        }
        .speaker_feature()
    };
    assert_eq!(feature(""), None);
    assert_eq!(feature("off"), None);
    assert_eq!(feature("name_only"), Some("speaker:name_only"));
    assert_eq!(feature("full"), Some("speaker:full"));
}

#[test]
fn bounded_map_evicts_oldest_and_survives_reinsertion() {
    let mut map = super::BoundedMap::new(2);
    map.insert("a", 1);
    map.insert("b", 2);
    map.insert("c", 3); // evicts a
    assert_eq!(map.remove(&"a"), None);
    assert_eq!(map.remove(&"b"), Some(2));

    // Re-insertion must not be evicted by its own stale order entry.
    let mut map = super::BoundedMap::new(2);
    map.insert("x", 1);
    map.remove(&"x");
    map.insert("x", 2);
    map.insert("y", 3);
    map.insert("z", 4); // evicts x (the live generation, as the oldest)
    assert_eq!(map.remove(&"x"), None);
    assert_eq!(map.remove(&"y"), Some(3));
    assert_eq!(map.remove(&"z"), Some(4));
}

#[tokio::test]
async fn other_members_churn_is_not_ours_to_report() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);
    inbound::install_handlers(shared.clone());

    let room_id = room_id!("!ops:localhost");
    server.sync_joined_room(&client, room_id).await;

    let someone_else = EventFactory::new()
        .room(room_id)
        .member(user_id!("@drew:localhost"))
        .membership(MembershipState::Leave);
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(someone_else.event_id(event_id!("$o1"))),
        )
        .await;

    assert!(
        frames.recv_timeout(QUIET).is_err(),
        "another member's leave is not a chat_membership frame"
    );
}

#[tokio::test]
async fn typing_pulse_and_stop_put_the_matching_states() {
    use wiremock::matchers::{body_partial_json, method, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (shared, frames) = test_shared(&client);

    let room_id = room_id!("!room:localhost");
    server.sync_joined_room(&client, room_id).await;
    // The pulse carries our 30 s server-side timeout; the stop carries
    // typing:false and no timeout — that is the whole difference between
    // an indicator that lingers and one that clears with the reply.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+$"))
        .and(body_partial_json(
            serde_json::json!({"typing": true, "timeout": 30000}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .named("typing pulse")
        .mount(server.server())
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+$"))
        .and(body_partial_json(serde_json::json!({"typing": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .named("typing stop")
        .mount(server.server())
        .await;

    outbound::typing(shared.clone(), room_id.to_string(), true).await;
    outbound::typing(shared, room_id.to_string(), false).await;

    server.server().verify().await;
    assert!(frames.recv_timeout(QUIET).is_err(), "typing owes no frames");
}
