use regex::Regex;
use std::sync::OnceLock;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

pub(crate) fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Strip HTML tags for plain-text fallback.
fn strip_html(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"<[^>]+>").unwrap());
    re.replace_all(s, "").into_owned()
}

// ── Markdown → Telegram HTML sanitizer ───────────────────────────────────────

/// Convert a Markdown table block (slice of raw lines) into bullet list lines.
fn table_to_bullets(rows: &[&str]) -> String {
    let mut out = String::new();
    let mut header_seen = false;
    for &line in rows {
        let trimmed = line.trim();
        // Separator row (e.g. |---|---|) → skip
        if trimmed.starts_with('|')
            && trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
        {
            header_seen = true; // next rows are data
            continue;
        }
        // Table row: split on '|', drop empty outer segments
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let cells: Vec<&str> = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect();
            if cells.is_empty() { continue; }
            // First row before separator = header → emit as bold label, not bullet
            if !header_seen {
                out.push_str(&format!("<b>{}</b>\n", cells.join(" — ")));
            } else {
                out.push_str(&format!("• {}\n", cells.join(" — ")));
            }
        }
    }
    out
}

/// Safety-net: convert residual Markdown bold (**text**) to <b>text</b>.
fn md_bold_to_html(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    re.replace_all(text, "<b>$1</b>").into_owned()
}

/// Safety-net: convert residual Markdown headers (## text) to <b>text</b>.
fn md_headers_to_html(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?m)^#{1,6} +(.+)$").unwrap());
    re.replace_all(text, "<b>$1</b>").into_owned()
}

/// Safety-net: convert residual inline `` `code` `` to <code>code</code>,
/// HTML-escaping the inner text so it renders verbatim.
fn md_inline_code_to_html(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"`([^`\n]+?)`").unwrap());
    re.replace_all(text, |caps: &regex::Captures| {
        format!("<code>{}</code>", escape_html(&caps[1]))
    })
    .into_owned()
}

/// Sanitize LLM output for Telegram HTML rendering:
/// 1. Convert fenced code blocks (```) → `<pre>…</pre>` (inner text escaped).
/// 2. Convert Markdown tables → bullet lists (Telegram has no `<table>` support).
/// 3. Convert residual `**bold**` → `<b>bold</b>`.
/// 4. Convert residual `## headers` → `<b>text</b>`.
/// 5. Convert residual inline `` `code` `` → `<code>code</code>`.
fn sanitize_for_telegram(text: &str) -> String {
    // Pass 1: block conversion (line-by-line state machine for fences & tables)
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // Fenced code block: ``` … ``` → <pre>escaped</pre>
        if trimmed.starts_with("```") {
            i += 1; // skip the opening fence (and any language tag)
            let start = i;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                i += 1;
            }
            let inner = lines[start..i].join("\n");
            if i < lines.len() { i += 1; } // skip the closing fence
            out.push_str("<pre>");
            out.push_str(&escape_html(&inner));
            out.push_str("</pre>\n");
            continue;
        }

        // Markdown table block
        if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
            let start = i;
            while i < lines.len() && {
                let t = lines[i].trim();
                (t.starts_with('|') && t.ends_with('|') && t.len() > 1)
                    || (t.starts_with('|')
                        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')))
            } {
                i += 1;
            }
            out.push_str(&table_to_bullets(&lines[start..i]));
        } else {
            out.push_str(line);
            out.push('\n');
            i += 1;
        }
    }

    // Passes 2-4: residual Markdown
    let out = md_bold_to_html(&out);
    let out = md_headers_to_html(&out);
    md_inline_code_to_html(&out)
}

/// Convert a tool label (our internal format with backtick-wrapped args) to
/// Telegram HTML: plain text is HTML-escaped, `` `code` `` becomes `<code>code</code>`.
pub(crate) fn label_to_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s;
    while let Some(open) = rest.find('`') {
        out.push_str(&escape_html(&rest[..open]));
        rest = &rest[open + 1..];
        if let Some(close) = rest.find('`') {
            out.push_str("<code>");
            out.push_str(&escape_html(&rest[..close]));
            out.push_str("</code>");
            rest = &rest[close + 1..];
        } else {
            // Unmatched backtick — emit as-is and stop.
            out.push('`');
            break;
        }
    }
    out.push_str(&escape_html(rest));
    out
}

// ── send_long ─────────────────────────────────────────────────────────────────

pub(crate) async fn send_long(bot: &Bot, chat_id: ChatId, text: &str, parse_mode: Option<ParseMode>) {
    const MAX: usize = 4000;
    if text.is_empty() { return; }
    // Sanitize before chunking so table blocks are never split mid-row.
    let sanitized;
    let text = if parse_mode == Some(ParseMode::Html) {
        sanitized = sanitize_for_telegram(text);
        &sanitized
    } else {
        text
    };
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    while start < chars.len() {
        let end   = (start + MAX).min(chars.len());
        let chunk: String = chars[start..end].iter().collect();
        let mut req = bot.send_message(chat_id, &chunk);
        if let Some(pm) = parse_mode { req = req.parse_mode(pm); }
        if req.await.is_err() {
            // Retry without parse_mode so the text reaches the user even if
            // the markup was malformed. Strip HTML tags first so we don't
            // display raw `<b>…</b>` to the user.
            let plain = strip_html(&chunk);
            bot.send_message(chat_id, plain).await.ok();
        }
        start = end;
    }
}
