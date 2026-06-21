//! Lightweight markdown rendering via `gtk::TextTag` — cursor-aware.
//!
//! The marker characters (`**`, `*`, `` ` ``, `#`, `- `) are tagged
//! `marker_hidden` (invisible). When the cursor enters their line, a
//! higher-priority `marker_visible` tag is applied to the same range,
//! overriding the invisibility so the user can edit. This gives the
//! "rendered while reading, raw while editing" experience without a
//! separate preview pane.

use gtk::prelude::*;
use gtk::{TextBuffer, TextTag};

const W_BOLD: i32 = 700;
const W_SEMI: i32 = 600;

pub struct MdTags {
    pub bold: TextTag,
    pub italic: TextTag,
    pub code: TextTag,
    pub code_block: TextTag,
    pub h1: TextTag,
    pub h2: TextTag,
    pub list: TextTag,
    /// Default style for syntax markers (`**`, `*`, `` ` ``, `#`, `- `).
    /// `invisible = true`, low priority — wins by default.
    pub marker_hidden: TextTag,
    /// Higher-priority override that re-shows markers on the cursor's
    /// line so the user can edit. Foreground is faded so they read as
    /// formatting hints, not content.
    pub marker_visible: TextTag,
}

impl MdTags {
    pub fn install(buffer: &TextBuffer) -> Self {
        let table = buffer.tag_table();
        let bold = TextTag::builder().name("md-bold").weight(W_BOLD).build();
        let italic = TextTag::builder()
            .name("md-italic")
            .style(gtk::pango::Style::Italic)
            .build();
        let code = TextTag::builder()
            .name("md-code")
            .family("monospace")
            .background("rgba(127,127,127,0.15)")
            .build();
        let code_block = TextTag::builder()
            .name("md-code-block")
            .family("monospace")
            .paragraph_background("rgba(127,127,127,0.10)")
            .left_margin(12)
            .pixels_above_lines(2)
            .pixels_below_lines(2)
            .build();
        let h1 = TextTag::builder()
            .name("md-h1")
            .weight(W_SEMI)
            .scale(1.45)
            .pixels_above_lines(6)
            .build();
        let h2 = TextTag::builder()
            .name("md-h2")
            .weight(W_SEMI)
            .scale(1.25)
            .pixels_above_lines(4)
            .build();
        let list = TextTag::builder().name("md-list").left_margin(18).build();
        let marker_hidden = TextTag::builder()
            .name("md-marker-hidden")
            .invisible(true)
            .build();
        let marker_visible = TextTag::builder()
            .name("md-marker-visible")
            .invisible(false)
            .foreground("rgba(127,127,127,0.55)")
            .weight(400) // pango Normal — neutralises any bold from underlying tag
            .style(gtk::pango::Style::Normal)
            .build();

        for t in [
            &bold,
            &italic,
            &code,
            &code_block,
            &h1,
            &h2,
            &list,
            &marker_hidden,
            &marker_visible,
        ] {
            table.add(t);
        }
        // Explicit priorities so `marker_visible` always wins over
        // `marker_hidden` regardless of insertion order.
        marker_hidden.set_priority(50);
        marker_visible.set_priority(table.size() - 1);

        Self {
            bold,
            italic,
            code,
            code_block,
            h1,
            h2,
            list,
            marker_hidden,
            marker_visible,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Bold,
    Italic,
    Code,
    CodeBlock,
    H1,
    H2,
    List,
}

#[derive(Clone)]
struct MdSpan {
    kind: Kind,
    full: (usize, usize),
    /// Marker char ranges to hide (e.g. `**` open + `**` close).
    markers: Vec<(usize, usize)>,
}

fn scan(text: &str) -> Vec<MdSpan> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();

    // Pass A: fenced code blocks first; mask their interior.
    let mut masked = vec![false; n];
    for (s, e) in find_fenced_code(&chars) {
        let mut markers = Vec::new();
        // Fence open: 3 backticks at start of line + optional language tag + newline
        let open_end = (s + 3 + chars[s + 3..].iter().take_while(|&&c| c != '\n').count()).min(e);
        markers.push((s, (open_end + 1).min(e))); // include newline if present
                                                  // Fence close: scan from end-of-block backward to ```
        let mut close_start = e;
        while close_start > 0
            && !(close_start + 2 < n
                && chars[close_start] == '`'
                && chars[close_start + 1] == '`'
                && chars[close_start + 2] == '`')
        {
            close_start -= 1;
            if close_start <= s + 3 {
                break;
            }
        }
        if close_start > s + 3 {
            markers.push((close_start, e));
        }
        out.push(MdSpan {
            kind: Kind::CodeBlock,
            full: (s, e),
            markers,
        });
        for k in s..e {
            if k < masked.len() {
                masked[k] = true;
            }
        }
    }

    let mut i = 0;
    let mut at_line_start = true;
    while i < n {
        if masked[i] {
            at_line_start = chars.get(i) == Some(&'\n');
            i += 1;
            continue;
        }
        let c = chars[i];

        if at_line_start {
            // Headings
            if c == '#' {
                let mut j = i;
                while j < n && chars[j] == '#' {
                    j += 1;
                }
                let level = j - i;
                if (level == 1 || level == 2) && j < n && chars[j] == ' ' {
                    let line_end = chars[i..]
                        .iter()
                        .position(|&c| c == '\n')
                        .map(|p| i + p)
                        .unwrap_or(n);
                    out.push(MdSpan {
                        kind: if level == 1 { Kind::H1 } else { Kind::H2 },
                        full: (i, line_end),
                        // Hide "# " or "## "
                        markers: vec![(i, j + 1)],
                    });
                    i = line_end;
                    at_line_start = true;
                    continue;
                }
            }
            // List bullet
            if (c == '-' || c == '*') && chars.get(i + 1) == Some(&' ') {
                let line_end = chars[i..]
                    .iter()
                    .position(|&c| c == '\n')
                    .map(|p| i + p)
                    .unwrap_or(n);
                out.push(MdSpan {
                    kind: Kind::List,
                    full: (i, line_end),
                    // Hide just the "- " / "* "
                    markers: vec![(i, i + 2)],
                });
                // Don't `continue` — let inline rules in the rest of the line scan too
            }
        }

        // Inline code
        if c == '`' && chars.get(i + 1) != Some(&'`') {
            if let Some(end) = find_close(&chars, i + 1, '`') {
                out.push(MdSpan {
                    kind: Kind::Code,
                    full: (i, end + 1),
                    markers: vec![(i, i + 1), (end, end + 1)],
                });
                i = end + 1;
                at_line_start = false;
                continue;
            }
        }

        // Bold **…**
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_close_pair(&chars, i + 2, '*') {
                out.push(MdSpan {
                    kind: Kind::Bold,
                    full: (i, end + 2),
                    markers: vec![(i, i + 2), (end, end + 2)],
                });
                i = end + 2;
                at_line_start = false;
                continue;
            }
        }

        // Italic *…* / _…_
        let italic_open = (c == '*' && chars.get(i + 1) != Some(&'*'))
            || (c == '_'
                && chars
                    .get(i.wrapping_sub(1))
                    .copied()
                    .is_none_or(|p| !p.is_alphanumeric()));
        if italic_open {
            if let Some(end) = find_close(&chars, i + 1, c) {
                if end > i + 1 {
                    out.push(MdSpan {
                        kind: Kind::Italic,
                        full: (i, end + 1),
                        markers: vec![(i, i + 1), (end, end + 1)],
                    });
                    i = end + 1;
                    at_line_start = false;
                    continue;
                }
            }
        }

        at_line_start = c == '\n';
        i += 1;
    }
    out
}

fn find_close(chars: &[char], from: usize, delim: char) -> Option<usize> {
    let mut k = from;
    while k < chars.len() {
        if chars[k] == '\n' {
            return None;
        }
        if chars[k] == delim {
            return Some(k);
        }
        k += 1;
    }
    None
}

fn find_close_pair(chars: &[char], from: usize, delim: char) -> Option<usize> {
    let mut k = from;
    while k + 1 < chars.len() {
        if chars[k] == '\n' && chars.get(k + 1) == Some(&'\n') {
            return None;
        }
        if chars[k] == delim && chars[k + 1] == delim {
            return Some(k);
        }
        k += 1;
    }
    None
}

fn find_fenced_code(chars: &[char]) -> Vec<(usize, usize)> {
    let n = chars.len();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut sol = true;
    while i < n {
        if sol && i + 2 < n && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            let open = i;
            let mut j = i + 3;
            while j < n && chars[j] != '\n' {
                j += 1;
            }
            if j < n {
                j += 1;
            }
            let mut k = j;
            let mut k_sol = true;
            let mut closed = false;
            while k < n {
                if k_sol
                    && k + 2 < n
                    && chars[k] == '`'
                    && chars[k + 1] == '`'
                    && chars[k + 2] == '`'
                {
                    let mut close_end = k + 3;
                    while close_end < n && chars[close_end] != '\n' {
                        close_end += 1;
                    }
                    spans.push((open, close_end));
                    i = close_end;
                    sol = false;
                    closed = true;
                    break;
                }
                k_sol = chars[k] == '\n';
                k += 1;
            }
            if !closed {
                spans.push((open, n));
                i = n;
                continue;
            }
        }
        if i < n {
            sol = chars[i] == '\n';
            i += 1;
        }
    }
    spans
}

/// Remove every markdown tag from the buffer, leaving the raw source: all
/// markers (`**`, `#`, …) visible and no styling. Backs the editor's
/// "show source" toggle, and is the strip step of `refresh_markdown_tags`.
pub fn clear_markdown_tags(buffer: &TextBuffer, tags: &MdTags) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    for t in [
        &tags.bold,
        &tags.italic,
        &tags.code,
        &tags.code_block,
        &tags.h1,
        &tags.h2,
        &tags.list,
        &tags.marker_hidden,
        &tags.marker_visible,
    ] {
        buffer.remove_tag(t, &start, &end);
    }
}

/// Strip then re-apply all markdown tags. Markers also get
/// `marker_hidden` so they vanish from rendering by default —
/// `reveal_markers_at_cursor` brings them back on the cursor's line.
pub fn refresh_markdown_tags(buffer: &TextBuffer, tags: &MdTags, excluded_tags: &[&TextTag]) {
    clear_markdown_tags(buffer, tags);

    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, true).to_string();
    let spans = scan(&text);

    for span in spans {
        let si = buffer.iter_at_offset(span.full.0 as i32);
        if excluded_tags.iter().any(|tag| si.has_tag(*tag)) {
            continue;
        }
        let ei = buffer.iter_at_offset(span.full.1 as i32);
        let style: &TextTag = match span.kind {
            Kind::Bold => &tags.bold,
            Kind::Italic => &tags.italic,
            Kind::Code => &tags.code,
            Kind::CodeBlock => &tags.code_block,
            Kind::H1 => &tags.h1,
            Kind::H2 => &tags.h2,
            Kind::List => &tags.list,
        };
        buffer.apply_tag(style, &si, &ei);

        for (ms, me) in span.markers {
            if me <= ms {
                continue;
            }
            let ms_iter = buffer.iter_at_offset(ms as i32);
            let me_iter = buffer.iter_at_offset(me as i32);
            buffer.apply_tag(&tags.marker_hidden, &ms_iter, &me_iter);
        }
    }
}

/// Re-show markers on the cursor's line so the user can see (and edit)
/// the raw markdown there. Takes only the two marker tags so callers
/// don't need to clone the whole `MdTags`.
pub fn reveal_markers_at_cursor(
    buffer: &TextBuffer,
    marker_hidden: &TextTag,
    marker_visible: &TextTag,
) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    buffer.remove_tag(marker_visible, &start, &end);

    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    let line = cursor.line();

    let line_start = buffer.iter_at_line(line).unwrap_or(start);
    let mut line_end = buffer
        .iter_at_line(line + 1)
        .unwrap_or_else(|| buffer.end_iter());
    if line_end.offset() > line_start.offset() {
        let _ = line_end.backward_char();
    }

    let mut iter = line_start;
    while iter.offset() <= line_end.offset() {
        if iter.has_tag(marker_hidden) {
            let run_start = iter;
            let mut run_end = iter;
            // Advance to the end of the contiguous marker run. We step forward
            // *before* re-checking the tag so the run always grows by at least
            // one char — even when the marker reaches the line's end (e.g. a
            // line ending in `**bold**`). The old version stopped with run_end
            // still inside the marker, so `iter = run_end` never advanced and
            // the outer loop spun forever at 100% CPU, freezing the app before
            // its window could even map.
            loop {
                if !run_end.forward_char() || run_end.offset() > line_end.offset() {
                    break;
                }
                if !run_end.has_tag(marker_hidden) {
                    break;
                }
            }
            if run_end.offset() > run_start.offset() {
                buffer.apply_tag(marker_visible, &run_start, &run_end);
                iter = run_end;
            } else if !iter.forward_char() {
                // No progress possible (end of buffer) — bail out.
                break;
            }
        } else if !iter.forward_char() {
            break;
        }
    }
}
