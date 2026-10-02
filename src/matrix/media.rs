//! Attachments, both directions (features `attachment_kinds`,
//! `sends_images`, `sends_files`; PLAN.md §3).
//!
//! Attachments travel by path (same-host convention): inbound media is
//! downloaded — and, in E2EE rooms, decrypted; the SDK does both in one
//! call — into the host-assigned `data_dir`, then referenced by path; the
//! host containment-checks and consumes the file. Outbound, the host hands
//! us a path to upload (`Room::send_attachment` encrypts in E2EE rooms).

use std::path::Path;
use std::sync::Arc;

use matrix_sdk::attachment::AttachmentConfig;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::room::reply::{EnforceThread, Reply as SdkReply};
use matrix_sdk::ruma::events::room::message::{
    AddMentions, MessageType, OriginalSyncRoomMessageEvent, ReplyWithinThread,
    TextMessageEventContent,
};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::sticker::OriginalSyncStickerEvent;
use matrix_sdk::ruma::EventId;
use matrix_sdk::{Room, RoomState};

use crate::proto::{Attachment, ConnFrame, MessageFromConn, SendFileFromHost, SendImageFromHost};
use crate::serve::{CommandOk, Responder};

use super::{entities, rooms, Shared, COMMAND_TIMEOUT};

/// What one Matrix media event wants ingested.
struct Ingest {
    source: MediaSource,
    kind: &'static str,
    mime_type: String,
    name: String,
    caption: String,
    declared_size: u64,
    duration_ms: i64,
}

/// Matrix v1.10 caption semantics: with a `filename` field, a differing
/// `body` is a user-written caption; otherwise `body` IS the filename.
fn name_and_caption(filename: Option<&str>, body: &str) -> (String, String) {
    match filename {
        Some(name) if name != body => (name.to_string(), body.to_string()),
        Some(name) => (name.to_string(), String::new()),
        None => (body.to_string(), String::new()),
    }
}

/// Map an `m.room.message` media msgtype onto an ingest plan. `None` =
/// not an attachment kind we translate (emote/notice/location stay out).
fn plan_ingest(msgtype: &MessageType) -> Option<Ingest> {
    match msgtype {
        MessageType::Image(c) => {
            let (name, caption) = name_and_caption(c.filename.as_deref(), &c.body);
            let info = c.info.as_deref();
            Some(Ingest {
                source: c.source.clone(),
                kind: "image",
                mime_type: info.and_then(|i| i.mimetype.clone()).unwrap_or_default(),
                name,
                caption,
                declared_size: info.and_then(|i| i.size).map(u64::from).unwrap_or(0),
                duration_ms: 0,
            })
        }
        MessageType::Audio(c) => {
            let (name, caption) = name_and_caption(c.filename.as_deref(), &c.body);
            let info = c.info.as_deref();
            Some(Ingest {
                source: c.source.clone(),
                // The MSC3245 voice marker is what distinguishes a spoken
                // voice note (transcribed host-side) from a music file.
                kind: if c.voice.is_some() { "voice" } else { "audio" },
                mime_type: info.and_then(|i| i.mimetype.clone()).unwrap_or_default(),
                name,
                caption,
                declared_size: info.and_then(|i| i.size).map(u64::from).unwrap_or(0),
                duration_ms: info
                    .and_then(|i| i.duration)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            })
        }
        MessageType::Video(c) => {
            let (name, caption) = name_and_caption(c.filename.as_deref(), &c.body);
            let info = c.info.as_deref();
            Some(Ingest {
                source: c.source.clone(),
                kind: "video",
                mime_type: info.and_then(|i| i.mimetype.clone()).unwrap_or_default(),
                name,
                caption,
                declared_size: info.and_then(|i| i.size).map(u64::from).unwrap_or(0),
                duration_ms: info
                    .and_then(|i| i.duration)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            })
        }
        MessageType::File(c) => {
            let (name, caption) = name_and_caption(c.filename.as_deref(), &c.body);
            let info = c.info.as_deref();
            Some(Ingest {
                source: c.source.clone(),
                kind: "document",
                mime_type: info.and_then(|i| i.mimetype.clone()).unwrap_or_default(),
                name,
                caption,
                declared_size: info.and_then(|i| i.size).map(u64::from).unwrap_or(0),
                duration_ms: 0,
            })
        }
        _ => None,
    }
}

/// A non-text `m.room.message` → one `message` frame carrying one typed
/// attachment. Caption rides the attachment (the host joins it to the
/// message text); an over-ceiling or failed download drops the message
/// with a `warn` — a bare frame with no text and no file helps nobody.
pub(crate) async fn handle_attachment_message(
    shared: Arc<Shared>,
    ev: OriginalSyncRoomMessageEvent,
    room: Room,
) {
    let Some(ingest) = plan_ingest(&ev.content.msgtype) else {
        return;
    };
    let shape =
        rooms::inbound_shape(&shared, &room, ev.content.relates_to.as_ref(), &ev.event_id).await;
    let replied_to_bot = !shape.reply_to.is_empty()
        && shared
            .sent_ids
            .lock()
            .expect("sent cache lock")
            .contains(&shape.reply_to);
    // No text to locate spans in — but an intentional mention (or a reply
    // to us) on a media message still matters to the host's group gate.
    let entities = entities::extract(
        "",
        None,
        ev.content.mentions.as_ref(),
        replied_to_bot,
        &shared.identity,
    );
    let attachment = match ingest_media(&shared, ingest, &ev.event_id).await {
        Ok(attachment) => attachment,
        Err(err) => {
            shared.sink.warn(format!(
                "dropping attachment {} in {}: {err}",
                ev.event_id,
                room.room_id()
            ));
            return;
        }
    };
    tracing::info!(
        event = %ev.event_id,
        chat = %shape.chat_id,
        kind = %attachment.kind,
        size = attachment.size,
        "attachment message delivered"
    );
    shared.sink.send(ConnFrame::Message(MessageFromConn {
        id: ev.event_id.to_string(),
        ts: i64::from(ev.origin_server_ts.get()),
        chat_id: shape.chat_id,
        chat_kind: shape.chat_kind,
        chat_title: shape.chat_title,
        parent_chat_id: shape.parent_chat_id,
        parent_chat_kind: shape.parent_chat_kind,
        user_id: ev.sender.to_string(),
        username: ev.sender.localpart().to_string(),
        reply_to: shape.reply_to,
        entities,
        attachments: vec![attachment],
        ..Default::default()
    }));
}

/// `m.sticker` is its own event type. Same pipeline, kind `sticker`; the
/// body is the sticker's alt text, delivered as its name.
pub(crate) async fn handle_sticker(shared: Arc<Shared>, ev: OriginalSyncStickerEvent, room: Room) {
    if room.state() != RoomState::Joined {
        return;
    }
    if Some(ev.sender.as_ref()) == shared.client.user_id() {
        return;
    }
    let info = &ev.content.info;
    let ingest = Ingest {
        source: ev.content.source.clone().into(),
        kind: "sticker",
        mime_type: info.mimetype.clone().unwrap_or_default(),
        name: ev.content.body.clone(),
        caption: String::new(),
        declared_size: info.size.map(u64::from).unwrap_or(0),
        duration_ms: 0,
    };
    let attachment = match ingest_media(&shared, ingest, &ev.event_id).await {
        Ok(attachment) => attachment,
        Err(err) => {
            shared.sink.warn(format!(
                "dropping sticker {} in {}: {err}",
                ev.event_id,
                room.room_id()
            ));
            return;
        }
    };
    let (chat_kind, chat_title) = rooms::chat_shape(&shared, &room).await;
    // Stickers are delivered room-scoped even inside a thread — record that
    // so later events about them correlate to the same chat.
    rooms::note_room_event(&shared, &ev.event_id);
    shared.sink.send(ConnFrame::Message(MessageFromConn {
        id: ev.event_id.to_string(),
        ts: i64::from(ev.origin_server_ts.get()),
        chat_id: room.room_id().to_string(),
        chat_kind,
        chat_title,
        user_id: ev.sender.to_string(),
        username: ev.sender.localpart().to_string(),
        attachments: vec![attachment],
        ..Default::default()
    }));
}

/// Download (+decrypt) into `data_dir` under a per-event unique name,
/// enforcing the size ceiling before AND after the transfer (`info.size`
/// is advisory — a lying event must not land 2 GB in `data_dir`).
async fn ingest_media(
    shared: &Shared,
    ingest: Ingest,
    event_id: &EventId,
) -> Result<Attachment, String> {
    let Some(data_dir) = &shared.data_dir else {
        return Err("the host assigned no data_dir".into());
    };
    let ceiling = shared.config.max_attachment_mb.saturating_mul(1024 * 1024);
    if ingest.declared_size > ceiling {
        return Err(format!(
            "declared size {} exceeds the {} MiB ceiling",
            ingest.declared_size, shared.config.max_attachment_mb
        ));
    }
    let request = MediaRequestParameters {
        source: ingest.source.clone(),
        format: MediaFormat::File,
    };
    // use_cache: false — one-shot host-consumed files must not bloat the
    // sqlite media cache.
    let data = shared
        .client
        .media()
        .get_media_content(&request, false)
        .await
        .map_err(|err| format!("download failed: {err}"))?;
    if data.len() as u64 > ceiling {
        return Err(format!(
            "actual size {} exceeds the {} MiB ceiling",
            data.len(),
            shared.config.max_attachment_mb
        ));
    }
    let size = data.len() as i64;
    let path = data_dir.join(unique_name(event_id.as_str(), &ingest.name));
    std::fs::create_dir_all(data_dir)
        .map_err(|err| format!("cannot create data_dir {}: {err}", data_dir.display()))?;
    std::fs::write(&path, data).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    let mime_type = if ingest.mime_type.is_empty() {
        guess_mime(Path::new(&ingest.name)).to_string()
    } else {
        ingest.mime_type
    };
    Ok(Attachment {
        mime_type,
        path: path.to_string_lossy().into_owned(),
        kind: ingest.kind.to_string(),
        name: ingest.name,
        size,
        duration_ms: ingest.duration_ms,
        caption: ingest.caption,
    })
}

/// `{event-id}-{filename}`, both parts reduced to a safe character set —
/// event ids are unique, so concurrent downloads never collide.
fn unique_name(event_id: &str, name: &str) -> String {
    let event = sanitize(event_id.trim_start_matches('$'));
    let mut name = sanitize(name);
    if name.is_empty() {
        name = "attachment".into();
    }
    // Keep the tail — that's where the extension lives.
    if name.chars().count() > 80 {
        let tail: String = name.chars().rev().take(80).collect();
        name = tail.chars().rev().collect();
    }
    format!("{event}-{name}")
}

fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `send_image` → upload + `m.image` (feature sends_images).
pub(crate) async fn send_image(shared: Arc<Shared>, cmd: SendImageFromHost, responder: Responder) {
    send_path(shared, cmd.chat_id, cmd.path, cmd.caption, responder).await
}

/// `send_file` → upload + `m.file` (feature sends_files).
pub(crate) async fn send_file(shared: Arc<Shared>, cmd: SendFileFromHost, responder: Responder) {
    send_path(shared, cmd.chat_id, cmd.path, cmd.caption, responder).await
}

/// One implementation serves both commands: `send_attachment` derives the
/// msgtype from the mime type (image/* → m.image, … else m.file), so the
/// extension-based guess routes each file to its natural rendering.
async fn send_path(
    shared: Arc<Shared>,
    chat_id: String,
    path: String,
    caption: String,
    responder: Responder,
) {
    let target = match rooms::resolve_chat(&shared, &chat_id) {
        Ok(target) => target,
        Err(err) => return responder.err(err),
    };
    let room = target.room.clone();
    let path = Path::new(&path);
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) => return responder.err(format!("cannot read {}: {err}", path.display())),
    };
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment");
    let mime = guess_mime(path);
    let mut config = AttachmentConfig::new();
    if !caption.is_empty() {
        config = config.caption(Some(TextMessageEventContent::plain(caption)));
    }
    if let Some(root) = &target.thread_root {
        // Thread-chat target: the upload rides the thread (fallback shape;
        // pointing the SDK at the root keeps it a plain thread message).
        config = config.reply(Some(SdkReply {
            event_id: root.clone(),
            enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
            add_mentions: AddMentions::No,
        }));
    }
    match tokio::time::timeout(
        COMMAND_TIMEOUT,
        room.send_attachment(filename, &mime, data, config),
    )
    .await
    {
        Err(_) => responder.err("attachment send timed out"),
        Ok(Err(err)) => responder.err(format!("attachment send failed: {err}")),
        Ok(Ok(response)) => {
            match &target.thread_root {
                Some(root) => rooms::note_thread_event(&shared, root, &response.event_id),
                None => rooms::note_room_event(&shared, &response.event_id),
            }
            responder.ok(CommandOk {
                message_id: response.event_id.to_string(),
                ..Default::default()
            });
        }
    }
}

/// Extension → mime. Modest by design: the host names its files sensibly,
/// and anything unknown degrades to octet-stream (rendered as m.file).
pub(crate) fn guess_mime(path: &Path) -> mime::Mime {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "m4a" | "aac" => "audio/aac",
        "flac" => "audio/flac",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        _ => "application/octet-stream",
    };
    mime.parse().expect("static mime strings parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_semantics_follow_matrix_1_10() {
        // No filename field: body IS the filename, no caption.
        assert_eq!(
            name_and_caption(None, "photo.jpg"),
            ("photo.jpg".into(), String::new())
        );
        // filename == body: still no caption.
        assert_eq!(
            name_and_caption(Some("photo.jpg"), "photo.jpg"),
            ("photo.jpg".into(), String::new())
        );
        // Differing body is a user-written caption.
        assert_eq!(
            name_and_caption(Some("photo.jpg"), "sunset at the pier"),
            ("photo.jpg".into(), "sunset at the pier".into())
        );
    }

    #[test]
    fn unique_names_are_safe_and_bounded() {
        let name = unique_name("$abc/DEF:123", "../../etc/passwd");
        assert_eq!(name, "abc_DEF_123-.._.._etc_passwd");
        assert!(!name.contains('/'));

        let long = "x".repeat(200) + ".png";
        let bounded = unique_name("$ev", &long);
        assert!(bounded.chars().count() <= 80 + "ev-".len());
        assert!(bounded.ends_with(".png"), "extension survives truncation");

        assert_eq!(unique_name("$ev", ""), "ev-attachment");
    }

    #[test]
    fn mime_guesses_cover_the_host_shapes() {
        assert_eq!(guess_mime(Path::new("a.png")), mime::IMAGE_PNG);
        assert_eq!(guess_mime(Path::new("a.JPG")).essence_str(), "image/jpeg");
        assert_eq!(
            guess_mime(Path::new("report.pdf")).essence_str(),
            "application/pdf"
        );
        assert_eq!(
            guess_mime(Path::new("no-extension")).essence_str(),
            "application/octet-stream"
        );
    }
}
