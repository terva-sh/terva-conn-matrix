//! Message markup extraction (feature `entities`, PLAN.md §3).
//!
//! `bot_mention` is the load-bearing kind — it drives terva's group
//! mention-gating. Intent comes from `m.mentions` (Matrix v1.7), legacy
//! `matrix.to`/`matrix:` pill anchors in the HTML `formatted_body`, or a
//! reply to one of our own messages; the span is then located by scanning
//! the plain `body`. Offsets and lengths are Unicode code points over the
//! delivered `text`; offset 0 length 0 means "mentioned, but not locatable"
//! (the host gate accepts that as a mention all the same).

use std::collections::{HashSet, VecDeque};

use matrix_sdk::ruma::events::Mentions;

use crate::proto::Entity;

/// Who the bot is, for span scanning. Built once at connect; the display
/// name is the global profile one (per-room overrides are rare enough to
/// fall through to the 0/0 "unlocatable" contract).
pub(crate) struct BotIdentity {
    pub user_id: String,
    pub localpart: String,
    pub display_name: Option<String>,
}

/// Bounded FIFO of our own outbound event ids — a reply to one of them
/// counts as a bot mention. Replies to messages sent before the cache
/// window (or a restart) still gate through `m.mentions`: clients include
/// the replied-to sender there since Matrix v1.7.
pub(crate) struct SentCache {
    ids: VecDeque<String>,
    set: HashSet<String>,
}

impl SentCache {
    const CAP: usize = 256;

    pub fn new() -> Self {
        SentCache {
            ids: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    pub fn insert(&mut self, id: String) {
        if self.set.insert(id.clone()) {
            self.ids.push_back(id);
            if self.ids.len() > Self::CAP {
                if let Some(old) = self.ids.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }
}

/// Entities for one inbound message. `text` is the delivered body (reply
/// fallback already stripped); `formatted_html` the HTML `formatted_body`
/// if present. At most one `bot_mention`; other users' pills become
/// `mention` entities when their span is locatable (an unlocatable plain
/// mention carries no information, unlike an unlocatable bot mention).
pub(crate) fn extract(
    text: &str,
    formatted_html: Option<&str>,
    mentions: Option<&Mentions>,
    replied_to_bot: bool,
    bot: &BotIdentity,
) -> Vec<Entity> {
    let pills = formatted_html.map(pills_in).unwrap_or_default();
    let mut out = Vec::new();

    let intentional =
        mentions.is_some_and(|m| m.user_ids.iter().any(|u| u.as_str() == bot.user_id));
    let bot_pill_text = pills
        .iter()
        .find(|p| p.user_id == bot.user_id)
        .map(|p| p.text.as_str());
    if intentional || bot_pill_text.is_some() || replied_to_bot {
        // Most-specific candidate first: the pill's literal anchor text is
        // what clients put in the body; then the explicit id forms; the
        // profile name last — servers commonly default it to the localpart,
        // where trying it early would find a strict substring of a typed
        // MXID and mis-anchor the span.
        let at_localpart = format!("@{}", bot.localpart);
        let mut candidates: Vec<&str> = Vec::new();
        candidates.extend(bot_pill_text);
        candidates.push(&bot.user_id);
        candidates.push(&at_localpart);
        candidates.extend(bot.display_name.as_deref().filter(|n| !n.is_empty()));
        candidates.push(&bot.localpart);
        let (offset, length) = candidates
            .iter()
            .find_map(|needle| find_span(text, needle))
            .unwrap_or((0, 0));
        out.push(Entity {
            kind: "bot_mention".into(),
            offset,
            length,
            ..Default::default()
        });
    }

    for pill in &pills {
        if pill.user_id == bot.user_id {
            continue;
        }
        if let Some((offset, length)) = find_span(text, &pill.text) {
            let entity = Entity {
                kind: "mention".into(),
                offset,
                length,
                user_id: pill.user_id.clone(),
            };
            // A user pilled twice resolves to the same first span — once is enough.
            if !out.contains(&entity) {
                out.push(entity);
            }
        }
    }
    out
}

/// First occurrence of `needle` in `text` as (offset, length) in Unicode
/// code points.
fn find_span(text: &str, needle: &str) -> Option<(i64, i64)> {
    if needle.is_empty() {
        return None;
    }
    let byte_ix = text.find(needle)?;
    let offset = text[..byte_ix].chars().count() as i64;
    let length = needle.chars().count() as i64;
    Some((offset, length))
}

struct Pill {
    user_id: String,
    text: String,
}

/// User pills in an HTML `formatted_body`: `<a href="…">name</a>` anchors
/// whose href is a `matrix.to` or `matrix:` user link. A rich-reply
/// fallback (`<mx-reply>`) pills the replied-to sender, so reply-to-bot is
/// caught here too for clients that still send fallbacks. Hand-rolled scan —
/// client-generated HTML is regular enough that a full parser buys nothing.
fn pills_in(html: &str) -> Vec<Pill> {
    // ASCII-lowercase keeps byte offsets aligned with the original
    // (to_lowercase() can change lengths); tags and attrs are ASCII.
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a") {
        let after = pos + rel + 2;
        pos = after;
        if !matches!(
            lower.as_bytes().get(after),
            Some(b' ' | b'\t' | b'\n' | b'>')
        ) {
            continue; // <abbr>, <article>, …
        }
        let Some(tag_end) = lower[after..].find('>').map(|i| after + i) else {
            break;
        };
        pos = tag_end + 1;
        let Some(href) = attr_value(&html[after..tag_end], "href") else {
            continue;
        };
        let Some(user_id) = user_from_uri(&href) else {
            continue;
        };
        let Some(close) = lower[pos..].find("</a>").map(|i| pos + i) else {
            break;
        };
        let text = unescape(&strip_tags(&html[pos..close]));
        pos = close + 4;
        let text = text.trim();
        if !text.is_empty() {
            out.push(Pill {
                user_id,
                text: text.to_string(),
            });
        }
    }
    out
}

/// The value of an HTML attribute inside a tag body (between `<a` and `>`).
fn attr_value(tag_body: &str, name: &str) -> Option<String> {
    let lower = tag_body.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(name) {
        let ix = from + rel;
        from = ix + name.len();
        if ix > 0 && !lower.as_bytes()[ix - 1].is_ascii_whitespace() {
            continue; // substring of another attr (data-href=…)
        }
        let rest = tag_body[ix + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let rest = &rest[1..];
        let end = rest.find(quote)?;
        return Some(rest[..end].to_string());
    }
    None
}

/// `https://matrix.to/#/@user:server` or `matrix:u/user:server` → the MXID.
fn user_from_uri(href: &str) -> Option<String> {
    if let Some(frag) = href.strip_prefix("https://matrix.to/#/") {
        let id = percent_decode(frag.split(['?', '/']).next().unwrap_or(""));
        return id.starts_with('@').then_some(id);
    }
    if let Some(rest) = href.strip_prefix("matrix:u/") {
        let id = percent_decode(rest.split(['?', '/']).next().unwrap_or(""));
        return (!id.is_empty()).then(|| format!("@{id}"));
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use matrix_sdk::ruma::owned_user_id;

    use super::*;

    fn bot() -> BotIdentity {
        BotIdentity {
            user_id: "@tervabot:localhost".into(),
            localpart: "tervabot".into(),
            display_name: Some("Terva Bot".into()),
        }
    }

    fn mentions_of(id: &str) -> Mentions {
        Mentions::with_user_ids([id.parse().unwrap()])
    }

    #[test]
    fn m_mentions_locates_the_display_name_span() {
        let ents = extract(
            "hey Terva Bot, deploy please",
            None,
            Some(&mentions_of("@tervabot:localhost")),
            false,
            &bot(),
        );
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].kind, "bot_mention");
        assert_eq!((ents[0].offset, ents[0].length), (4, 9));
    }

    #[test]
    fn offsets_are_code_points_not_bytes() {
        // "héllo → " is 8 code points but more bytes.
        let ents = extract(
            "héllo → @tervabot go",
            None,
            Some(&mentions_of("@tervabot:localhost")),
            false,
            &bot(),
        );
        assert_eq!((ents[0].offset, ents[0].length), (8, 9));
    }

    #[test]
    fn unlocatable_mention_is_present_at_zero_zero() {
        let ents = extract(
            "no textual trace here",
            None,
            Some(&mentions_of("@tervabot:localhost")),
            false,
            &bot(),
        );
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].kind, "bot_mention");
        assert_eq!((ents[0].offset, ents[0].length), (0, 0));
    }

    #[test]
    fn a_typed_mxid_wins_over_a_localpart_shaped_display_name() {
        // Synapse defaults a fresh account's display name to its localpart;
        // it must not anchor the span one short inside a typed MXID.
        let bot = BotIdentity {
            user_id: "@tervabot:localhost".into(),
            localpart: "tervabot".into(),
            display_name: Some("tervabot".into()),
        };
        let ents = extract(
            "hey @tervabot:localhost go",
            None,
            Some(&mentions_of("@tervabot:localhost")),
            false,
            &bot,
        );
        assert_eq!((ents[0].offset, ents[0].length), (4, 19));
    }

    #[test]
    fn mentions_of_someone_else_are_not_a_bot_mention() {
        let ents = extract(
            "hey Terva Bot in name only",
            None,
            Some(&mentions_of("@other:localhost")),
            false,
            &bot(),
        );
        assert!(ents.is_empty());
    }

    #[test]
    fn legacy_pill_yields_bot_mention_with_the_anchor_text_span() {
        let html = r#"<a href="https://matrix.to/#/%40tervabot%3Alocalhost">Terva Bot</a>: hi"#;
        let ents = extract("Terva Bot: hi", Some(html), None, false, &bot());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].kind, "bot_mention");
        assert_eq!((ents[0].offset, ents[0].length), (0, 9));
    }

    #[test]
    fn matrix_uri_scheme_pill_is_honored() {
        let html = r#"<a href='matrix:u/tervabot:localhost'>Terva Bot</a> hello"#;
        let ents = extract("Terva Bot hello", Some(html), None, false, &bot());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].kind, "bot_mention");
    }

    #[test]
    fn other_user_pills_become_mention_entities() {
        let html = concat!(
            r#"ask <a href="https://matrix.to/#/@drew:localhost">drew</a> and "#,
            r#"<a href="https://matrix.to/#/@tervabot:localhost">Terva Bot</a>"#,
        );
        let ents = extract("ask drew and Terva Bot", Some(html), None, false, &bot());
        assert_eq!(ents.len(), 2);
        assert_eq!(ents[0].kind, "bot_mention");
        assert_eq!((ents[0].offset, ents[0].length), (13, 9));
        assert_eq!(ents[1].kind, "mention");
        assert_eq!(ents[1].user_id, "@drew:localhost");
        assert_eq!((ents[1].offset, ents[1].length), (4, 4));
    }

    #[test]
    fn unlocatable_plain_mentions_are_dropped() {
        // Reply-fallback pills reference text that was stripped from body.
        let html = r#"<mx-reply><blockquote><a href="https://matrix.to/#/@third:localhost">third</a> said x</blockquote></mx-reply>actual"#;
        let ents = extract("actual", Some(html), None, false, &bot());
        assert!(ents.is_empty());
    }

    #[test]
    fn reply_fallback_pill_to_the_bot_reads_as_bot_mention() {
        let html = r#"<mx-reply><blockquote><a href="https://matrix.to/#/@tervabot:localhost">Terva Bot</a> earlier</blockquote></mx-reply>ok do it"#;
        let ents = extract("ok do it", Some(html), None, false, &bot());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].kind, "bot_mention");
        assert_eq!((ents[0].offset, ents[0].length), (0, 0));
    }

    #[test]
    fn replied_to_bot_flag_alone_is_a_mention() {
        let ents = extract("sounds good", None, None, true, &bot());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].kind, "bot_mention");
        assert_eq!((ents[0].offset, ents[0].length), (0, 0));
    }

    #[test]
    fn no_signals_no_entities() {
        let ents = extract(
            "just chatting about tervabot's uptime",
            None,
            None,
            false,
            &bot(),
        );
        // The name appearing in text without m.mentions/pill/reply intent is
        // NOT a mention — plain-text scanning is the host's fallback, not ours.
        assert!(ents.is_empty());
    }

    #[test]
    fn duplicate_pills_dedupe() {
        let html = concat!(
            r#"<a href="https://matrix.to/#/@drew:localhost">drew</a> "#,
            r#"<a href="https://matrix.to/#/@drew:localhost">drew</a>"#,
        );
        let ents = extract("drew drew", Some(html), None, false, &bot());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].kind, "mention");
    }

    #[test]
    fn html_entities_in_anchor_text_unescape() {
        let html = r#"<a href="https://matrix.to/#/@amp:localhost">A &amp; B</a>"#;
        let ents = extract("ping A & B now", Some(html), None, false, &bot());
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].user_id, "@amp:localhost");
        assert_eq!((ents[0].offset, ents[0].length), (5, 5));
    }

    #[test]
    fn non_user_matrix_to_links_are_ignored() {
        let html = r#"see <a href="https://matrix.to/#/!room:localhost?via=x">the room</a>"#;
        let ents = extract("see the room", Some(html), None, false, &bot());
        assert!(ents.is_empty());
    }

    #[test]
    fn sent_cache_is_bounded_and_evicts_oldest() {
        let mut cache = SentCache::new();
        for i in 0..(SentCache::CAP + 10) {
            cache.insert(format!("$ev{i}"));
        }
        assert!(!cache.contains("$ev0"), "oldest evicted");
        assert!(
            !cache.contains("$ev9"),
            "everything past the window evicted"
        );
        assert!(cache.contains("$ev10"), "newest CAP ids retained");
        assert!(cache.contains(&format!("$ev{}", SentCache::CAP + 9)));
    }

    #[test]
    fn mentions_builder_shape_matches_ruma() {
        // Pin the ruma surface the extraction relies on.
        let m = Mentions::with_user_ids([owned_user_id!("@tervabot:localhost")]);
        assert!(m
            .user_ids
            .iter()
            .any(|u| u.as_str() == "@tervabot:localhost"));
    }
}
