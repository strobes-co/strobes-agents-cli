//! A small full-screen Ratatui list selector, used to pick a thread or
//! workspace before entering chat. Runs its own terminal init/restore so it
//! composes with the main chat loop (which inits its own terminal afterward).

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, MouseEventKind};
use futures_util::StreamExt;
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

/// Show a selectable list. Returns the chosen index, or None if cancelled.
pub async fn select(title: &str, items: &[String]) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut terminal = ratatui::init();
    let mut events = EventStream::new();
    let mut state = ListState::default();
    state.select(Some(0));

    let result = loop {
        terminal.draw(|f| {
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
                .split(f.area());
            let list_items: Vec<ListItem> = items
                .iter()
                .map(|s| ListItem::new(Line::from(s.clone())))
                .collect();
            let list = List::new(list_items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {title} "))
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("➤ ");
            f.render_stateful_widget(list, chunks[0], &mut state);
            f.render_widget(
                Paragraph::new(" ↑/↓ move · Enter select · Esc cancel")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        })?;

        match events.next().await {
            Some(Ok(Event::Key(k))) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Up | KeyCode::Char('k') => move_sel(&mut state, items.len(), -1),
                KeyCode::Down | KeyCode::Char('j') => move_sel(&mut state, items.len(), 1),
                KeyCode::Enter => break state.selected(),
                KeyCode::Esc | KeyCode::Char('q') => break None,
                _ => {}
            },
            Some(Ok(Event::Mouse(m))) => match m.kind {
                MouseEventKind::ScrollUp => move_sel(&mut state, items.len(), -1),
                MouseEventKind::ScrollDown => move_sel(&mut state, items.len(), 1),
                _ => {}
            },
            Some(Err(_)) | None => break None,
            _ => {}
        }
    };

    ratatui::restore();
    Ok(result)
}

fn move_sel(state: &mut ListState, len: usize, delta: i32) {
    let cur = state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    state.select(Some(next));
}
