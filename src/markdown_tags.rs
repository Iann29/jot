//! Lightweight markdown rendering via `gtk::TextTag`.
//!
//! NOT a parser — a single-pass scanner that emits `(kind, char_start, char_end)`
//! tuples and applies one tag per match. Markers (`**`, `*`, `` ` ``, `#`) stay
//! visible so editing remains plain-text and the cursor can sit between them.

use gtk::prelude::*;
use gtk::{TextBuffer, TextTag};

const W_BOLD: i32 = 700; // pango::Weight::Bold
const W_SEMI: i32 = 600; // pango::Weight::Semibold

pub struct MdTags {
    pub bold: TextTag,
    pub italic: TextTag,
    pub code: TextTag,
    pub code_block: TextTag,
    pub h1: TextTag,
    pub h2: TextTag,
    pub list: TextTag,
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
        for t in [&bold, &italic, &code, &code_block, &h1, &h2, &list] {
            table.add(t);
        }
        Self {
            bold,
            italic,
            code,
            code_block,
            h1,
            h2,
            list,
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

/// Scan once, emit (kind, char_start, char_end). Char offsets, not bytes.
fn scan(text: &str) -> Vec<(Kind, usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();

    // Pass A: fenced code blocks first; mask their interior so other rules ignore it.
    let mut masked = vec![false; n];
    let fences = find_fenced_code(&chars);
    for (s, e) in &fences {
        out.push((Kind::CodeBlock, *s, *e));
        for k in *s..*e {
            if k < masked.len() {
                masked[k] = true;
            }
        }
    }

    let mut i = 0usize;
    let mut at_line_start = true;

    while i < n {
        if masked[i] {
            at_line_start = chars.get(i) == Some(&'\n');
            i += 1;
            continue;
        }
        let c = chars[i];

        if at_line_start {
            // Headings — `#` or `##` followed by a space at the start of a line.
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
                    out.push(
                        (
                            if level == 1 { Kind::H1 } else { Kind::H2 },
                            i,
                            line_end,
                        ),
                    );
                    i = line_end;
                    at_line_start = true;
                    continue;
                }
            }
            // List item — `- ` or `* ` at start of line. Tag the whole line
            // (left margin only); inline rules still apply within.
            if (c == '-' || c == '*') && chars.get(i + 1) == Some(&' ') {
                let line_end = chars[i..]
                    .iter()
                    .position(|&c| c == '\n')
                    .map(|p| i + p)
                    .unwrap_or(n);
                out.push((Kind::List, i, line_end));
                // fall through so bold/italic inside the list line still scans
            }
        }

        // Inline code: `…` (single backtick).
        if c == '`' && chars.get(i + 1) != Some(&'`') {
            if let Some(end) = find_close(&chars, i + 1, '`') {
                out.push((Kind::Code, i, end + 1));
                i = end + 1;
                at_line_start = false;
                continue;
            }
        }

        // Bold: **…**
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_close_pair(&chars, i + 2, '*') {
                out.push((Kind::Bold, i, end + 2));
                i = end + 2;
                at_line_start = false;
                continue;
            }
        }

        // Italic: *…*  or  _…_  (not part of ** or snake_case).
        let italic_open = (c == '*' && chars.get(i + 1) != Some(&'*'))
            || (c == '_'
                && chars
                    .get(i.wrapping_sub(1))
                    .copied()
                    .map_or(true, |p| !p.is_alphanumeric()));
        if italic_open {
            if let Some(end) = find_close(&chars, i + 1, c) {
                if end > i + 1 {
                    out.push((Kind::Italic, i, end + 1));
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

/// ``` at start of line opens a fenced block; ``` at start of line closes it.
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

/// Strip and re-apply all markdown tags across the buffer. Cheap to call
/// at end of `save_pending` / `load_body_into_buffer`.
pub fn refresh_markdown_tags(buffer: &TextBuffer, tags: &MdTags, image_tag: &TextTag) {
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
    ] {
        buffer.remove_tag(t, &start, &end);
    }

    let text = buffer.text(&start, &end, true).to_string();
    let spans = scan(&text);

    for (kind, s, e) in spans {
        let si = buffer.iter_at_offset(s as i32);
        if si.has_tag(image_tag) {
            continue; // skip invisible image-markdown regions
        }
        let ei = buffer.iter_at_offset(e as i32);
        let tag: &TextTag = match kind {
            Kind::Bold => &tags.bold,
            Kind::Italic => &tags.italic,
            Kind::Code => &tags.code,
            Kind::CodeBlock => &tags.code_block,
            Kind::H1 => &tags.h1,
            Kind::H2 => &tags.h2,
            Kind::List => &tags.list,
        };
        buffer.apply_tag(tag, &si, &ei);
    }
}
