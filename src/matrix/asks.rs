//! Reaction-widget asks (feature `asks`, PLAN.md §3).
//!
//! Matrix has no buttons, so an ask renders as its text plus an emoji
//! legend, and the bot seeds its own message with one reaction per option —
//! a user taps a seed to answer. A reaction is a signed Matrix event whose
//! sender the origin homeserver authenticated, so answers grade `attested`,
//! with one honest caveat: a malicious *federated* homeserver can forge
//! events for its own users only, and an MXID embeds its server — so the
//! exact-MXID `restrict_to` match below (which the host re-checks anyway)
//! keeps grants sound unless the owner's own homeserver is hostile.
//!
//! `ask_close` withdraws the seed reactions and renders the outcome into
//! the question message; `expires_ms` withdraws the widget on a timer
//! (late taps fall through as plain reactions). Multi-select and
//! custom-answer asks never reach us — the host routes those to its
//! numbered-text floor by design.

use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::ruma::events::reaction::{OriginalSyncReactionEvent, ReactionEventContent};
use matrix_sdk::ruma::events::relation::Annotation;
use matrix_sdk::ruma::events::room::message::{ReplacementMetadata, RoomMessageEventContent};
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
use matrix_sdk::{Room, RoomState};

use crate::proto::{AnswerFromConn, AskCloseFromHost, AskFromHost, AskOption, ConnFrame};
use crate::serve::{CommandOk, Responder};

use super::{rooms, Shared, COMMAND_TIMEOUT};

/// Seed reactions and their redactions are widget furniture, not the
/// command itself — each gets a short budget and failures degrade the
/// widget (the numbered legend still reads) instead of failing the ask.
const FURNITURE_TIMEOUT: Duration = Duration::from_secs(5);

/// One open (or expired-but-unclosed) ask.
pub(crate) struct AskState {
    pub room_id: OwnedRoomId,
    pub message_id: OwnedEventId,
    /// (normalized emoji, option key) in render order.
    pub options: Vec<(String, String)>,
    pub restrict_to: Vec<String>,
    pub seed_events: Vec<OwnedEventId>,
    /// The original ask text, for rendering the outcome on close.
    pub text: String,
    /// Expired asks stop translating answers (their seeds are gone) but
    /// stay registered so a late `ask_close` can still render the outcome.
    pub expired: bool,
}

/// Emoji keys arrive with or without VARIATION SELECTOR-16 depending on
/// the client — strip it so a picker-typed 👍️ matches our seeded 👍.
fn normalize_key(key: &str) -> String {
    key.chars().filter(|c| *c != '\u{FE0F}').collect()
}

/// One emoji per option: the option's `hint` when present and not already
/// taken, else the next free circled digit (①②③…). Returns normalized
/// emojis, parallel to `options`.
fn assign_emojis(options: &[AskOption]) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    let mut fallback = 0u32;
    for option in options {
        let hint = normalize_key(option.hint.trim());
        let emoji = if !hint.is_empty() && !used.contains(&hint) {
            hint
        } else {
            loop {
                // ① is U+2460; twenty exist. Past that (degenerate ask),
                // reuse the last — the legend still numbers correctly.
                let candidate = char::from_u32(0x2460 + fallback.min(19))
                    .expect("circled digits exist")
                    .to_string();
                fallback += 1;
                if !used.contains(&candidate) || fallback > 20 {
                    break candidate;
                }
            }
        };
        used.push(emoji.clone());
    }
    used
}

/// The ask body: question text, blank line, one `- emoji label` per option.
/// The legend carries the labels (a bare reaction row explains nothing) and
/// doubles as the numbered fallback for anyone who cannot see reactions.
fn render_ask(text: &str, options: &[AskOption], emojis: &[String]) -> String {
    let mut body = text.to_string();
    body.push_str("\n\n");
    for (option, emoji) in options.iter().zip(emojis) {
        body.push_str(&format!("- {emoji} {}\n", option.label));
    }
    body
}

/// `ask` → post the question, register the widget, seed one reaction per
/// option on our own message. The result acknowledges the RENDERING (the
/// posted message id), not any human.
pub(crate) async fn handle_ask(shared: Arc<Shared>, cmd: AskFromHost, responder: Responder) {
    let target = match rooms::resolve_chat(&shared, &cmd.chat_id) {
        Ok(target) => target,
        Err(err) => return responder.err(err),
    };
    if cmd.options.is_empty() {
        return responder.err("ask carries no options");
    }
    let emojis = assign_emojis(&cmd.options);
    let body = render_ask(&cmd.text, &cmd.options, &emojis);
    let mut content = RoomMessageEventContent::text_markdown(body);
    // An ask into a thread chat renders inside the thread.
    content.relates_to = rooms::outbound_relation(&shared, &target, &cmd.reply_to);
    let room = target.room;
    let message_id = match tokio::time::timeout(COMMAND_TIMEOUT, room.send(content)).await {
        Err(_) => return responder.err("ask send timed out"),
        Ok(Err(err)) => return responder.err(format!("ask send failed: {err}")),
        Ok(Ok(sent)) => sent.response.event_id.clone(),
    };
    match &target.thread_root {
        Some(root) => rooms::note_thread_event(&shared, root, &message_id),
        None => rooms::note_room_event(&shared, &message_id),
    }
    // Registered before seeding so a tap racing the seeds still answers.
    shared.asks.lock().expect("asks lock").insert(
        cmd.id.clone(),
        AskState {
            room_id: room.room_id().to_owned(),
            message_id: message_id.clone(),
            options: emojis
                .iter()
                .cloned()
                .zip(cmd.options.iter().map(|o| o.key.clone()))
                .collect(),
            restrict_to: cmd.restrict_to.clone(),
            seed_events: Vec::new(),
            text: cmd.text.clone(),
            expired: false,
        },
    );
    for emoji in &emojis {
        let content = ReactionEventContent::new(Annotation::new(message_id.clone(), emoji.clone()));
        match tokio::time::timeout(FURNITURE_TIMEOUT, room.send(content)).await {
            Ok(Ok(sent)) => {
                if let Some(state) = shared.asks.lock().expect("asks lock").get_mut(&cmd.id) {
                    state.seed_events.push(sent.response.event_id.clone());
                }
            }
            // A missing seed degrades the widget, not the ask — the legend
            // still lists the option and a manual reaction still answers.
            Ok(Err(err)) => tracing::warn!("seeding {emoji} on ask {} failed: {err}", cmd.id),
            Err(_) => tracing::warn!("seeding {emoji} on ask {} timed out", cmd.id),
        }
    }
    tracing::info!(ask = %cmd.id, message = %message_id, "ask opened and seeded");
    responder.ok(CommandOk {
        message_id: message_id.to_string(),
        ..Default::default()
    });
}

/// A reaction landing on an open ask message. Returns true when consumed
/// (an answer, or a filtered imposter tap); false lets the caller deliver
/// it as a plain reaction (unknown emoji, expired ask).
pub(crate) async fn try_answer(
    shared: &Shared,
    ev: &OriginalSyncReactionEvent,
    room: &Room,
) -> bool {
    let key = normalize_key(&ev.content.relates_to.key);
    let target = &ev.content.relates_to.event_id;
    let (ask_id, option_key, allowed) = {
        let asks = shared.asks.lock().expect("asks lock");
        let Some((ask_id, state)) = asks.iter().find(|(_, s)| &s.message_id == target) else {
            return false;
        };
        if state.expired {
            return false;
        }
        let Some(option_key) = state
            .options
            .iter()
            .find(|(emoji, _)| *emoji == key)
            .map(|(_, k)| k.clone())
        else {
            return false;
        };
        let allowed = state.restrict_to.is_empty()
            || state.restrict_to.iter().any(|u| u == ev.sender.as_str());
        (ask_id.clone(), option_key, allowed)
    };
    // Whatever happens next, this reaction is widget traffic — its later
    // redaction must not surface as a message_deleted.
    shared
        .answer_events
        .lock()
        .expect("answer events lock")
        .insert(ev.event_id.clone(), ());
    if !allowed {
        tracing::info!(
            "ignoring answer to ask {ask_id} from {} (not in restrict_to)",
            ev.sender
        );
        // Best-effort: make the stray tap disappear so the widget stays
        // readable. Needs redact power over others' events; failure is fine.
        if let Err(err) = tokio::time::timeout(
            FURNITURE_TIMEOUT,
            room.redact(&ev.event_id, Some("not addressed to you"), None),
        )
        .await
        .map_err(|_| "timed out".to_string())
        .and_then(|r| r.map(|_| ()).map_err(|e| e.to_string()))
        {
            tracing::debug!("could not redact imposter tap: {err}");
        }
        return true;
    }
    // A Matrix reaction is a signed event with an authenticated sender —
    // attested (see the module docs for the federation caveat).
    tracing::info!(ask = %ask_id, sender = %ev.sender, "answer delivered");
    shared.sink.send(ConnFrame::Answer(AnswerFromConn {
        ask_id,
        key: option_key,
        user_id: ev.sender.to_string(),
        username: ev.sender.localpart().to_string(),
        attestation: "attested".into(),
    }));
    true
}

/// `ask_close` → withdraw the seed reactions and render the outcome into
/// the question message (the audit trail lives in the channel).
pub(crate) async fn handle_ask_close(
    shared: Arc<Shared>,
    cmd: AskCloseFromHost,
    responder: Responder,
) {
    let state = shared.asks.lock().expect("asks lock").remove(&cmd.ask_id);
    let Some(state) = state else {
        // Session-local state: an ask opened before a restart cannot be
        // closed by us. The host's own lifecycle copes with the error.
        return responder.err(format!("unknown ask {:?}", cmd.ask_id));
    };
    let Some(room) = shared.client.get_room(&state.room_id) else {
        return responder.err(format!("ask room {} is gone", state.room_id));
    };
    if room.state() != RoomState::Joined {
        return responder.err(format!("no longer joined to {}", state.room_id));
    }
    tracing::info!(ask = %cmd.ask_id, "ask closed; withdrawing the widget");
    redact_seeds(&room, &state.seed_events).await;
    if !cmd.outcome.is_empty() {
        let body = format!("{}\n\n**{}**", state.text, cmd.outcome);
        let content = RoomMessageEventContent::text_markdown(body)
            .make_replacement(ReplacementMetadata::new(state.message_id.clone(), None));
        match tokio::time::timeout(COMMAND_TIMEOUT, room.send(content)).await {
            Err(_) => return responder.err("outcome edit timed out"),
            Ok(Err(err)) => return responder.err(format!("outcome edit failed: {err}")),
            Ok(Ok(_)) => {}
        }
    }
    responder.ok(CommandOk::default());
}

/// The `expires_ms` timer fired: withdraw the widget but keep the state
/// (marked expired) so a late `ask_close` still renders its outcome.
/// Retries briefly first — the dispatch-time timer can beat a slow ask
/// registration when the expiry is degenerately short.
pub(crate) async fn expire(shared: Arc<Shared>, ask_id: String) {
    let mut lookups = 0;
    let (room_id, seeds) = loop {
        {
            let mut asks = shared.asks.lock().expect("asks lock");
            if let Some(state) = asks.get_mut(&ask_id) {
                if state.expired {
                    return;
                }
                state.expired = true;
                break (
                    state.room_id.clone(),
                    std::mem::take(&mut state.seed_events),
                );
            }
        }
        lookups += 1;
        if lookups > 30 {
            return; // the ask never registered (its send failed) or closed
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    tracing::info!("ask {ask_id} expired; withdrawing the widget");
    if let Some(room) = shared.client.get_room(&room_id) {
        redact_seeds(&room, &seeds).await;
    }
}

async fn redact_seeds(room: &Room, seeds: &[OwnedEventId]) {
    for seed in seeds {
        match tokio::time::timeout(FURNITURE_TIMEOUT, room.redact(seed, None, None)).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => tracing::warn!("withdrawing seed reaction {seed} failed: {err}"),
            Err(_) => tracing::warn!("withdrawing seed reaction {seed} timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(key: &str, label: &str, hint: &str) -> AskOption {
        AskOption {
            key: key.into(),
            label: label.into(),
            hint: hint.into(),
            ..Default::default()
        }
    }

    #[test]
    fn hints_win_and_fallbacks_fill_gaps() {
        let emojis = assign_emojis(&[
            option("approve", "Approve", "👍"),
            option("deny", "Deny", ""),
            option("later", "Later", "👍"), // duplicate hint → fallback
        ]);
        assert_eq!(emojis, vec!["👍", "①", "②"]);
    }

    #[test]
    fn variation_selectors_normalize_away() {
        assert_eq!(normalize_key("👍\u{FE0F}"), "👍");
        let emojis = assign_emojis(&[option("a", "A", "👍\u{FE0F}")]);
        assert_eq!(emojis, vec!["👍"]);
    }

    #[test]
    fn legend_renders_labels_with_their_emojis() {
        let options = [
            option("approve", "Approve", "👍"),
            option("deny", "Deny", ""),
        ];
        let emojis = assign_emojis(&options);
        let body = render_ask("Run `rm -rf build/`?", &options, &emojis);
        assert_eq!(body, "Run `rm -rf build/`?\n\n- 👍 Approve\n- ① Deny\n");
    }
}
