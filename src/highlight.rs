//! Shell command/response syntax highlighting — shared by the `tower console` TUI
//! (`tui.rs`) and the `tower gateway` remote-shell pane (`gateway/tui.rs`) so the two
//! never drift. Pure `&str -> ratatui spans`, no state.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const COL_PATH: Color = Color::Cyan;
const COL_CMD: Color = Color::Yellow;
const COL_KEY: Color = Color::Magenta;
const COL_VAL: Color = Color::Green;
const COL_PUNCT: Color = Color::DarkGray;

/// Highlight a shell command line: `/system/eeprom print level=3` →
/// path segments cyan, `/` separators dim, bare words yellow, `key=value` magenta/green.
pub(crate) fn command(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, tok) in line.split_inclusive(' ').enumerate() {
        let (word, trail) = match tok.strip_suffix(' ') {
            Some(w) => (w, " "),
            None => (tok, ""),
        };
        if word.is_empty() {
            // collapsed runs of spaces
        } else if word.contains('=') {
            let (k, v) = word.split_once('=').unwrap();
            spans.push(Span::styled(k.to_string(), Style::new().fg(COL_KEY)));
            spans.push(Span::styled("=".to_string(), Style::new().fg(COL_PUNCT)));
            spans.push(Span::styled(v.to_string(), Style::new().fg(COL_VAL)));
        } else if word.contains('/') {
            for part in word.split_inclusive('/') {
                let (seg, sep) = match part.strip_suffix('/') {
                    Some(sg) => (sg, "/"),
                    None => (part, ""),
                };
                if !seg.is_empty() {
                    spans.push(Span::styled(seg.to_string(), Style::new().fg(COL_PATH)));
                }
                if !sep.is_empty() {
                    spans.push(Span::styled(sep.to_string(), Style::new().fg(COL_PUNCT)));
                }
            }
        } else if i == 0 {
            // First token without a slash: still an address into the tree.
            spans.push(Span::styled(word.to_string(), Style::new().fg(COL_PATH)));
        } else {
            spans.push(Span::styled(word.to_string(), Style::new().fg(COL_CMD)));
        }
        if !trail.is_empty() {
            spans.push(Span::raw(" "));
        }
    }
    spans
}

/// Highlight a shell-response line: command-syntax lines (starting with `/`, e.g. `/export`
/// output) reuse [`command`]; `key: value` / `key = value` lines split into a magenta key,
/// dim separator, and green value; anything else renders raw.
pub(crate) fn response(line: &str) -> Line<'static> {
    let l = line;
    if l.starts_with('/') {
        return Line::from(command(l));
    }
    if l.starts_with('>') {
        // The local echo of the command the user sent.
        let mut spans = vec![Span::styled("> ".to_string(), Style::new().fg(COL_PUNCT))];
        spans.extend(command(l.trim_start_matches("> ")));
        return Line::from(spans);
    }
    for sep in [" = ", ": "] {
        if let Some((k, v)) = l.split_once(sep) {
            // Only treat it as key/value when the key looks like one (single-ish word).
            if !k.is_empty() && k.len() <= 24 && !k.contains("  ") {
                return Line::from(vec![
                    Span::styled(k.to_string(), Style::new().fg(COL_KEY)),
                    Span::styled(sep.to_string(), Style::new().fg(COL_PUNCT)),
                    Span::styled(v.to_string(), Style::new().fg(COL_VAL)),
                ]);
            }
        }
    }
    Line::raw(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_paints_path_and_kv() {
        let spans = command("/system/settings set addr=random");
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "/system/settings set addr=random");
        // The key/value split produced distinct spans.
        assert!(spans.iter().any(|s| s.content.as_ref() == "addr"));
        assert!(spans.iter().any(|s| s.content.as_ref() == "random"));
    }

    #[test]
    fn response_splits_key_value_and_passes_raw() {
        let kv = response("temp-period = 60");
        let txt: String = kv.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(txt, "temp-period = 60");
        assert!(kv.spans.len() >= 3, "key / sep / value");
        // A plain line with no separator survives verbatim as one raw span.
        let raw = response("just some text");
        let txt: String = raw.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(txt, "just some text");
    }
}
