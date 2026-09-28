//! Text hygiene for anything that came off the wire.
//!
//! Every address and string value is passed through [`clean`] before it is
//! stored, so the rest of the program only ever sees text that is bounded in
//! length and cannot carry terminal control sequences.

use std::fmt::Write as _;

/// Maximum number of characters kept for addresses and string values.
pub const MAX_TEXT_CHARS: usize = 256;

const ELLIPSIS: char = '…';

/// Returns true for characters that must never reach the terminal verbatim.
fn needs_escape(c: char) -> bool {
    c.is_control()                                // C0, DEL, C1 (includes ESC and CSI)
        || matches!(c,
            '\u{061C}'                            // arabic letter mark
            | '\u{200B}'..='\u{200F}'             // zero-width chars, LRM/RLM
            | '\u{2028}' | '\u{2029}'             // line/paragraph separator
            | '\u{202A}'..='\u{202E}'             // bidi embeddings/overrides
            | '\u{2066}'..='\u{2069}'             // bidi isolates
            | '\u{FEFF}'                          // BOM / zero-width no-break space
            | '\u{FFF9}'..='\u{FFFB}') // interlinear annotation controls
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c if (c as u32) <= 0xFF => {
            let _ = write!(out, "\\x{:02x}", c as u32);
        }
        c => {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
    }
}

/// Escapes control/formatting characters and truncates to [`MAX_TEXT_CHARS`].
pub fn clean(input: &str) -> String {
    clean_max(input, MAX_TEXT_CHARS)
}

/// Like [`clean`] with an explicit character budget (always at least 1).
/// The result never exceeds `max_chars` characters; a trailing `…` marks
/// truncation.
pub fn clean_max(input: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut out = String::with_capacity(input.len().min(max_chars * 4));
    let mut used = 0usize;
    let mut piece = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        piece.clear();
        if needs_escape(c) {
            push_escaped(&mut piece, c);
        } else {
            piece.push(c);
        }
        let n = piece.chars().count();
        // Reserve one char for the ellipsis unless this is the final piece.
        let is_last = chars.peek().is_none();
        let budget = if is_last { max_chars } else { max_chars - 1 };
        if used + n > budget {
            out.push(ELLIPSIS);
            return out;
        }
        out.push_str(&piece);
        used += n;
    }
    out
}

/// Short hex preview of a blob: `blob[len] 0a1b2c…`.
pub fn blob_preview(bytes: &[u8]) -> String {
    const PREVIEW: usize = 16;
    let mut s = format!("blob[{}]", bytes.len());
    if !bytes.is_empty() {
        s.push(' ');
        for b in bytes.iter().take(PREVIEW) {
            let _ = write!(s, "{b:02x}");
        }
        if bytes.len() > PREVIEW {
            s.push(ELLIPSIS);
        }
    }
    s
}

/// VRChat ID prefixes that are redacted in exports.
const ID_PREFIXES: [&str; 4] = ["avtr_", "usr_", "wrld_", "grp_"];

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// Replaces anything that looks like a VRChat ID (`avtr_…`, `usr_…`,
/// `wrld_…`, `grp_…`, matched case-insensitively) with `<prefix>[redacted]`.
pub fn redact_ids(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    'outer: while !rest.is_empty() {
        for prefix in ID_PREFIXES {
            let Some(head) = rest.get(..prefix.len()) else { continue };
            if !head.eq_ignore_ascii_case(prefix) {
                continue;
            }
            let tail = &rest[prefix.len()..];
            let id_len: usize = tail.chars().take_while(|&c| is_id_char(c)).map(char::len_utf8).sum();
            if id_len > 0 {
                out.push_str(head);
                out.push_str("[redacted]");
                rest = &tail[id_len..];
                continue 'outer;
            }
        }
        let Some(c) = rest.chars().next() else { break };
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}
