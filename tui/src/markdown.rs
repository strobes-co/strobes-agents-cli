//! Minimal Markdown → ratatui renderer built on pulldown-cmark.
//!
//! Unlike `tui-markdown` (which keeps the literal `#`/list markers), this
//! strips the markup and applies terminal styling: headings render bold and
//! colored (no `#`), `**bold**`/`_italic_`/`` `code` `` become styled inline
//! spans, lists get `•`/`n.` prefixes, code blocks are dimmed, block quotes
//! get a `▏` gutter. Returns fully-owned ('static) lines.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

#[derive(Default, Clone, Copy)]
struct Inline {
    bold: u32,
    italic: u32,
    code: bool,
    strike: u32,
}

impl Inline {
    fn style(&self) -> Style {
        let mut s = Style::default();
        if self.code {
            return Style::default().fg(Color::Yellow);
        }
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        s.fg(Color::White)
    }
}

enum ListKind {
    Bullet,
    Ordered(u64),
}

/// Find bare http(s) URLs in a string (no regex dependency).
pub fn find_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | ')' | ']' | '}'))
                .unwrap_or(rest.len());
            let mut url = rest[..end].to_string();
            // strip common trailing punctuation
            while url.ends_with(['.', ',', ';', ':', '!', '?']) {
                url.pop();
            }
            if url.len() > 8 {
                out.push(url.clone());
            }
            i += end.max(1);
        } else {
            // advance by one char boundary
            i += 1;
            while i < text.len() && (bytes[i] & 0xC0) == 0x80 {
                i += 1;
            }
        }
    }
    out
}

/// Extract (label, url) link pairs from Markdown — both `[text](url)` links
/// and bare URLs in the prose.
pub fn extract_links(md: &str) -> Vec<(String, String)> {
    let parser = Parser::new(md);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur_url: Option<String> = None;
    let mut cur_label = String::new();
    for ev in parser {
        match ev {
            Event::Start(Tag::Link { dest_url, .. }) => {
                cur_url = Some(dest_url.to_string());
                cur_label.clear();
            }
            Event::End(TagEnd::Link) => {
                if let Some(u) = cur_url.take() {
                    let label = if cur_label.trim().is_empty() { u.clone() } else { cur_label.trim().to_string() };
                    out.push((label, u));
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if cur_url.is_some() {
                    cur_label.push_str(&t);
                } else {
                    for u in find_urls(&t) {
                        out.push((u.clone(), u));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn render(md: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(md, opts);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut inline = Inline::default();
    let mut heading: Option<HeadingLevel> = None;
    let mut in_code_block = false;
    let mut lists: Vec<ListKind> = Vec::new();
    let mut quote_depth: u32 = 0;

    // Table accumulation state.
    let mut in_table = false;
    let mut in_cell = false;
    let mut cell_buf = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut header_rows = 0usize;

    // Indent/prefix for the current block (list bullets, quote gutters).
    let line_prefix = |lists: &[ListKind], quote: u32| -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for _ in 0..quote {
            spans.push(Span::styled("▏ ".to_string(), Style::default().fg(Color::DarkGray)));
        }
        // indentation for nested lists
        if lists.len() > 1 {
            spans.push(Span::raw("  ".repeat(lists.len() - 1)));
        }
        spans
    };

    let flush = |lines: &mut Vec<Line<'static>>, cur: &mut Vec<Span<'static>>| {
        lines.push(Line::from(std::mem::take(cur)));
    };

    for ev in parser {
        match ev {
            // ----- tables -----
            Event::Start(Tag::Table(_)) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                in_table = true;
                table_rows.clear();
                header_rows = 0;
            }
            Event::Start(Tag::TableHead) => row.clear(),
            Event::End(TagEnd::TableHead) => {
                table_rows.push(std::mem::take(&mut row));
                header_rows = 1;
            }
            Event::Start(Tag::TableRow) => row.clear(),
            Event::End(TagEnd::TableRow) => table_rows.push(std::mem::take(&mut row)),
            Event::Start(Tag::TableCell) => {
                in_cell = true;
                cell_buf.clear();
            }
            Event::End(TagEnd::TableCell) => {
                in_cell = false;
                row.push(cell_buf.trim().to_string());
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                render_table(&mut lines, &table_rows, header_rows);
                table_rows.clear();
                lines.push(Line::default());
            }

            Event::Start(Tag::Heading { level, .. }) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                if !lines.is_empty() {
                    lines.push(Line::default()); // blank line before heading
                }
                heading = Some(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                let (color, _) = heading_style(heading.unwrap_or(HeadingLevel::H3));
                // Restyle the accumulated spans as a heading.
                let styled: Vec<Span<'static>> = cur
                    .drain(..)
                    .map(|s| Span::styled(s.content.into_owned(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD)))
                    .collect();
                lines.push(Line::from(styled));
                heading = None;
            }

            Event::Start(Tag::Paragraph) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                cur.extend(line_prefix(&lists, quote_depth));
            }
            Event::End(TagEnd::Paragraph) => {
                flush(&mut lines, &mut cur);
                if lists.is_empty() {
                    lines.push(Line::default()); // paragraph spacing only outside lists
                }
            }

            Event::Start(Tag::List(start)) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                lists.push(match start {
                    Some(n) => ListKind::Ordered(n),
                    None => ListKind::Bullet,
                });
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                if lists.is_empty() {
                    lines.push(Line::default());
                }
            }
            Event::Start(Tag::Item) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                cur.extend(line_prefix(&lists, quote_depth));
                let marker = match lists.last_mut() {
                    Some(ListKind::Ordered(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                cur.push(Span::styled(marker, Style::default().fg(Color::Cyan)));
            }
            Event::End(TagEnd::Item) => {
                flush(&mut lines, &mut cur);
            }

            Event::Start(Tag::BlockQuote(_)) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                quote_depth = quote_depth.saturating_sub(1);
            }

            Event::Start(Tag::CodeBlock(_kind)) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                in_code_block = true;
                let _ = &_kind; // kind (fenced/indented) unused
                let _: CodeBlockKind = _kind;
            }
            Event::End(TagEnd::CodeBlock) => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                in_code_block = false;
            }

            Event::Start(Tag::Strong) => inline.bold += 1,
            Event::End(TagEnd::Strong) => inline.bold = inline.bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => inline.italic += 1,
            Event::End(TagEnd::Emphasis) => inline.italic = inline.italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => inline.strike += 1,
            Event::End(TagEnd::Strikethrough) => inline.strike = inline.strike.saturating_sub(1),

            Event::Start(Tag::Link { .. }) => {}
            Event::End(TagEnd::Link) => {}

            Event::Text(t) => {
                if in_cell {
                    cell_buf.push_str(&t);
                } else if in_code_block {
                    // Code blocks may contain multiple lines.
                    let mut first = true;
                    for piece in t.split('\n') {
                        if !first {
                            flush(&mut lines, &mut cur);
                        }
                        first = false;
                        cur.push(Span::styled(
                            format!("    {piece}"),
                            Style::default().fg(Color::Green),
                        ));
                    }
                } else {
                    cur.push(Span::styled(t.into_string(), inline.style()));
                }
            }
            Event::Code(t) => {
                if in_cell {
                    cell_buf.push_str(&t);
                } else {
                    cur.push(Span::styled(t.into_string(), Style::default().fg(Color::Yellow)));
                }
            }
            Event::SoftBreak => {
                if in_cell {
                    cell_buf.push(' ');
                } else {
                    cur.push(Span::raw(" "));
                }
            }
            Event::HardBreak => {
                if in_cell {
                    cell_buf.push(' ');
                } else {
                    flush(&mut lines, &mut cur);
                }
            }
            Event::Rule => {
                if !cur.is_empty() {
                    flush(&mut lines, &mut cur);
                }
                lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    // Collapse runs of blank lines to a single blank (Claude-Code-style compact
    // spacing) and trim leading/trailing blanks.
    let mut compact: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut prev_blank = true; // drop leading blanks
    for l in lines {
        let blank = l.spans.is_empty() || l.spans.iter().all(|s| s.content.trim().is_empty());
        if blank && prev_blank {
            continue;
        }
        prev_blank = blank;
        compact.push(l);
    }
    while matches!(compact.last(), Some(l) if l.spans.is_empty() || l.spans.iter().all(|s| s.content.trim().is_empty())) {
        compact.pop();
    }
    compact
}

/// Render accumulated table rows as aligned, separated columns.
fn render_table(lines: &mut Vec<Line<'static>>, rows: &[Vec<String>], header_rows: usize) {
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    for (ri, r) in rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ".to_string())];
        for i in 0..cols {
            let c = r.get(i).map(|s| s.as_str()).unwrap_or("");
            let pad = widths[i].saturating_sub(c.chars().count());
            let cell = format!("{c}{}", " ".repeat(pad));
            let style = if ri < header_rows {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            spans.push(Span::styled(cell, style));
            if i + 1 < cols {
                spans.push(Span::styled(" │ ".to_string(), Style::default().fg(Color::DarkGray)));
            }
        }
        lines.push(Line::from(spans));
        if header_rows > 0 && ri + 1 == header_rows {
            let total: usize = widths.iter().sum::<usize>() + 3 * cols.saturating_sub(1);
            lines.push(Line::from(Span::styled(
                format!("  {}", "─".repeat(total)),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
}

fn heading_style(level: HeadingLevel) -> (Color, ()) {
    let color = match level {
        HeadingLevel::H1 | HeadingLevel::H2 => Color::Cyan,
        _ => Color::LightBlue,
    };
    (color, ())
}
