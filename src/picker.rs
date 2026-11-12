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

/// Braille spinner frames for the loading overlay.
const LOADER_SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Draw a centered, animated "Loading…" overlay on top of whatever is already
/// on screen. `frame` advances the spinner and the growing dots. The box width
/// is fixed (dots are padded) so it never jitters as it animates.
pub fn draw_loader(f: &mut ratatui::Frame, label: &str, frame: usize) {
    let area = f.area();
    let spin = LOADER_SPIN[frame % LOADER_SPIN.len()];
    let dots = ".".repeat(1 + (frame / 3) % 3); // 1..=3 dots
    // Left-justify the dots to a fixed width of 3 so the line length is stable.
    let body = format!("{spin}  {label}{dots:<3}");
    let inner_w = body.chars().count() as u16 + 2; // one space padding each side
    let w = inner_w
        .saturating_add(2) // borders
        .min(area.width.saturating_sub(2))
        .max(18);
    let h = 3u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect { x, y, width: w, height: h };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let para = Paragraph::new(Line::from(Span::styled(
        format!(" {body} "),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )))
    .block(block)
    .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(Clear, rect);
    f.render_widget(para, rect);
}

/// Run `fut` to completion while showing the centered loading overlay, so a
/// multi-second fetch animates instead of leaving the screen frozen or blank.
/// Returns the future's output. Draw errors during the wait are non-fatal.
pub async fn with_loader<F, T>(terminal: &mut ratatui::DefaultTerminal, label: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    let mut frame = 0usize;
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = ticker.tick() => {
                let _ = terminal.draw(|f| draw_loader(f, label, frame));
                frame = frame.wrapping_add(1);
            }
        }
    }
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

/// A snapshot of a live picker's rows. `labels` is what's shown; `ids` and
/// `flags` run parallel to it and are opaque to the picker — the caller reads
/// them back by the returned index's id/flag so a background refresh that
/// reorders rows can never desync the selection from its underlying item.
#[derive(Clone, Default)]
pub struct LiveList {
    pub labels: Vec<String>,
    pub ids: Vec<String>,
    pub flags: Vec<bool>,
    pub auth: String,
}

/// What the user did on a live picker. Item/Shortcut carry the selected row's
/// id + flag captured atomically at selection time (not an index), so they stay
/// correct even if fresh data arrived a moment earlier.
pub enum LiveNav {
    Item { id: String, flag: bool },
    Shortcut { id: String, flag: bool },
    Back,
    Quit,
}

/// Like `select_with`, but the rows come from a `watch` channel: the picker
/// paints the current snapshot immediately (e.g. from cache) and redraws in
/// place whenever a background refetch publishes a newer snapshot — WITHOUT
/// blocking, and without touching anything once the user has selected (the loop
/// simply returns). The user can act on cached rows the instant they appear.
pub async fn select_live(
    terminal: &mut ratatui::DefaultTerminal,
    title: &str,
    mut rx: tokio::sync::watch::Receiver<LiveList>,
) -> Result<LiveNav> {
    let mut events = EventStream::new();
    let mut state = ListState::default();
    state.select(Some(0));
    let mut filter = String::new();
    // Once the refetch task has sent its update and dropped the sender,
    // `rx.changed()` resolves immediately forever — this gate disables that
    // branch after the channel closes so the loop never spins.
    let mut watch_live = true;

    let result = loop {
        // Stable snapshot for this iteration — cloned so a mid-iteration update
        // can't desync labels from ids.
        let view = rx.borrow_and_update().clone();
        let items = &view.labels;

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
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
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
                    Paragraph::new(banner_lines(&view.auth)),
                    Rect { x: body.x, y: body.y, width: body.width, height: top_h },
                );
            }
            let list_items: Vec<ListItem> =
                visible.iter().map(|&i| ListItem::new(Line::from(items[i].clone()))).collect();
            let search = if filter.is_empty() { String::new() } else { format!(" — search: {filter}") };
            let list = List::new(list_items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {title} ({}){search} ", visible.len()))
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .highlight_style(
                    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("➤ ");
            f.render_stateful_widget(list, list_area, &mut state);
            f.render_widget(
                Paragraph::new(" type to search · ↑/↓ move · Enter select · Tab shortcut · Esc back · ^C quit")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        })?;

        let id_at = |orig: usize| view.ids.get(orig).cloned().unwrap_or_default();
        let flag_at = |orig: usize| view.flags.get(orig).copied().unwrap_or(false);

        tokio::select! {
            // A background refresh published newer rows — just loop and redraw.
            // Disabled once the sender is gone so it can't busy-spin.
            changed = rx.changed(), if watch_live => {
                if changed.is_err() {
                    // Sender dropped: keep showing the last snapshot, stop polling.
                    watch_live = false;
                }
            }
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Char('c') if ctrl => break LiveNav::Quit,
                        KeyCode::Up => move_sel(&mut state, visible.len(), -1),
                        KeyCode::Down => move_sel(&mut state, visible.len(), 1),
                        KeyCode::Enter => {
                            if let Some(&orig) = visible.get(state.selected().unwrap_or(0)) {
                                break LiveNav::Item { id: id_at(orig), flag: flag_at(orig) };
                            }
                        }
                        KeyCode::Tab => {
                            if let Some(&orig) = visible.get(state.selected().unwrap_or(0)) {
                                break LiveNav::Shortcut { id: id_at(orig), flag: flag_at(orig) };
                            }
                        }
                        KeyCode::Esc => break LiveNav::Back,
                        KeyCode::Backspace => { filter.pop(); state.select(Some(0)); }
                        KeyCode::Char(c) if !ctrl => { filter.push(c); state.select(Some(0)); }
                        _ => {}
                    }
                }
                Some(Ok(Event::Mouse(m))) => match m.kind {
                    MouseEventKind::ScrollUp => move_sel(&mut state, visible.len(), -1),
                    MouseEventKind::ScrollDown => move_sel(&mut state, visible.len(), 1),
                    _ => {}
                },
                Some(Err(_)) | None => break LiveNav::Quit,
                _ => {}
            }
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
