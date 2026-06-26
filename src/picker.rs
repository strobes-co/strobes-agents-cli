//! A small full-screen Ratatui list selector, used to pick a thread or
//! workspace before entering chat. Runs its own terminal init/restore so it
//! composes with the main chat loop (which inits its own terminal afterward).

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

/// Outcome of a picker interaction.
pub enum Nav {
    /// The user selected the row at this index.
    Item(usize),
    /// The user pressed Esc/q (go back / cancel — caller decides what that means).
    Back,
    /// The user pressed ^C or the input stream ended (quit the app).
    Quit,
    /// The user pressed Tab while item at this index was highlighted.
    /// Callers that don't handle shortcuts should treat this as `Back`.
    Shortcut(usize),
}

/// Show a selectable list on its own terminal (enters/leaves the alternate
/// screen). Use this for standalone, one-off pickers.
pub async fn select(title: &str, items: &[String]) -> Result<Nav> {
    let mut terminal = ratatui::init();
    let r = select_with(&mut terminal, title, items, "").await;
    ratatui::restore();
    r
}

/// Show a selectable list reusing an already-initialized terminal, so chained
/// pickers (workspace → thread → chat) don't flash the normal screen between
/// them. Returns what the user did (select / back / quit).
pub async fn select_with(
    terminal: &mut ratatui::DefaultTerminal,
    title: &str,
    items: &[String],
    auth: &str,
) -> Result<Nav> {
    if items.is_empty() {
        return Ok(Nav::Back);
    }
    let mut events = EventStream::new();
    let mut state = ListState::default();
    state.select(Some(0));
    // Type-to-search filter over item labels (case-insensitive substring).
    let mut filter = String::new();

    let result = loop {
        // Indices of items matching the current filter.
        let needle = filter.to_lowercase();
        let visible: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, s)| needle.is_empty() || s.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        let sel = state.selected().unwrap_or(0).min(visible.len().saturating_sub(1));
        state.select(Some(sel));

        terminal.draw(|f| {
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
                .split(f.area());
            let body = chunks[0];
            let rows = visible.len().max(1) as u16;
            let h = rows.saturating_add(2).clamp(3, body.height.max(3));
            let list_area = Rect {
                x: body.x,
                y: body.y + body.height.saturating_sub(h),
                width: body.width,
                height: h,
            };
            let top_h = body.height.saturating_sub(h);
            if top_h >= 3 {
                f.render_widget(
                    Paragraph::new(banner_lines(auth)),
                    Rect { x: body.x, y: body.y, width: body.width, height: top_h },
                );
            }
            let list_items: Vec<ListItem> =
                visible.iter().map(|&i| ListItem::new(Line::from(items[i].clone()))).collect();
            let search = if filter.is_empty() {
                String::new()
            } else {
                format!(" — search: {filter}")
            };
            let list = List::new(list_items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {title} ({}){search} ", visible.len()))
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("➤ ");
            f.render_stateful_widget(list, list_area, &mut state);
            f.render_widget(
                Paragraph::new(" type to search · ↑/↓ move · Enter select · Tab shortcut · Esc back · ^C quit")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        })?;

        match events.next().await {
            Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Char('c') if ctrl => break Nav::Quit,
                    KeyCode::Up => move_sel(&mut state, visible.len(), -1),
                    KeyCode::Down => move_sel(&mut state, visible.len(), 1),
                    KeyCode::Enter => {
                        if let Some(&orig) = visible.get(state.selected().unwrap_or(0)) {
                            break Nav::Item(orig);
                        }
                    }
                    KeyCode::Tab => {
                        if let Some(&orig) = visible.get(state.selected().unwrap_or(0)) {
                            break Nav::Shortcut(orig);
                        }
                    }
                    KeyCode::Esc => break Nav::Back,
                    KeyCode::Backspace => {
                        filter.pop();
                        state.select(Some(0));
                    }
                    KeyCode::Char(c) if !ctrl => {
                        filter.push(c);
                        state.select(Some(0));
                    }
                    _ => {}
                }
            }
            Some(Ok(Event::Mouse(m))) => match m.kind {
                MouseEventKind::ScrollUp => move_sel(&mut state, visible.len(), -1),
                MouseEventKind::ScrollDown => move_sel(&mut state, visible.len(), 1),
                _ => {}
            },
            Some(Err(_)) | None => break Nav::Quit,
            _ => {}
        }
    };

    Ok(result)
}

/// Banner lines (cyan art + an optional green "authenticated …" subtitle).
fn banner_lines(auth: &str) -> Vec<Line<'static>> {
    // Left indent + a blank top line so the art isn't jammed into the corner.
    const LEFT: &str = "   ";
    let mut v: Vec<Line<'static>> = vec![Line::default()];
    v.extend(crate::app::BANNER.lines().map(|l| {
        Line::from(Span::styled(format!("{LEFT}{l}"), Style::default().fg(Color::Cyan)))
    }));
    // `auth` may carry extra context lines (e.g. the selected workspace) joined
    // with '\n'; the first is the auth line (green), the rest are cyan context.
    for (i, line) in auth.split('\n').filter(|l| !l.is_empty()).enumerate() {
        let style = if i == 0 {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        };
        v.push(Line::from(Span::styled(format!("{LEFT}{line}"), style)));
    }
    v
}

/// Prompt for one line of text on the shared terminal (bottom-anchored input
/// box, banner above). Returns the entered text, or None if cancelled (Esc/^C).
pub async fn prompt_text(
    terminal: &mut ratatui::DefaultTerminal,
    title: &str,
    initial: &str,
    auth: &str,
) -> Result<Option<String>> {
    let mut events = EventStream::new();
    let mut input = initial.to_string();
    let result = loop {
        terminal.draw(|f| {
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
            let body = chunks[0];
            let h = 3u16;
            let top_h = body.height.saturating_sub(h);
            if top_h >= 3 {
                f.render_widget(
                    Paragraph::new(banner_lines(auth)),
                    Rect { x: body.x, y: body.y, width: body.width, height: top_h },
                );
            }
            let ia = Rect { x: body.x, y: body.y + body.height.saturating_sub(h), width: body.width, height: h };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} "))
                .border_style(Style::default().fg(Color::Green));
            f.render_widget(Clear, ia);
            f.render_widget(Paragraph::new(format!("› {input}")).block(block), ia);
            f.render_widget(
                Paragraph::new(" type · Enter submit · Esc cancel").style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        })?;
        match events.next().await {
            Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Char('c') if ctrl => break None,
                    KeyCode::Enter => break Some(input.clone()),
                    KeyCode::Esc => break None,
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => input.push(c),
                    _ => {}
                }
            }
            Some(Err(_)) | None => break None,
            _ => {}
        }
    };
    Ok(result)
}

fn move_sel(state: &mut ListState, len: usize, delta: i32) {
    let cur = state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    state.select(Some(next));
}
