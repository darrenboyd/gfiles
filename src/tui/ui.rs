//! Drawing. Reads `App` state; the only thing it writes back is `page_rows`.
//!
//! ```text
//! ┌ gfiles ──────────────────────────────────────────────┐
//! │ > tolk                                               │
//! └──────────────────────────────────────────────────────┘
//! ┌ Kind ───────┐┌ 3 results ────────────────────────────┐
//! │ all       3 ││ ▶ book     1.1 MB  gaffer:home  Books/…│
//! │ book      1 ││   document   4 KB  gaffer:home  Docs/…│
//! └─────────────┘└───────────────────────────────────────┘
//! ┌ Details ─────────────────────────────────────────────┐
//! │ gaffer:/home/darren/Books/Tolkien/The Hobbit.epub    │
//! │ book · 1.1 MB · modified 2025-03-02 · volume home    │
//! └──────────────────────────────────────────────────────┘
//!  type to search  ↑↓ move  Tab kind  Enter print path  Esc clear/quit
//! ```

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use super::app::App;
use crate::fmt;

const KIND_WIDTH: u16 = 9;
const SIZE_WIDTH: u16 = 9;
const HIGHLIGHT: &str = "▶ ";
const COLUMN_GAP: u16 = 2;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [search, middle, details, help] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [kinds, results] =
        Layout::horizontal([Constraint::Length(20), Constraint::Min(20)]).areas(middle);

    draw_search(frame, app, search);
    draw_kinds(frame, app, kinds);
    draw_results(frame, app, results);
    draw_details(frame, app, details);
    frame.render_widget(
        Line::from(" type to search  ↑↓ move  Tab kind  Enter print path & quit  Esc clear/quit")
            .dark_gray(),
        help,
    );
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::from("> ").cyan(),
        Span::from(app.query.as_str()),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::bordered().title(" gfiles ")),
        area,
    );
    let cursor_x = area.x + 3 + app.query.chars().count() as u16;
    frame.set_cursor_position((cursor_x.min(area.right() - 2), area.y + 1));
}

fn draw_kinds(frame: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .kinds
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let count = app.count_for(i);
            let text = format!(" {kind:<9}{count:>8} ");
            let style = if i == app.kind_idx {
                Style::new().add_modifier(Modifier::REVERSED)
            } else if count == 0 {
                Style::new().fg(Color::DarkGray)
            } else {
                Style::new()
            };
            Line::styled(text, style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Kind ")),
        area,
    );
}

fn draw_results(frame: &mut Frame, app: &mut App, area: Rect) {
    let total = app.total();
    let title = if (app.hits.len() as i64) < total {
        format!(" {} of {total} results ", app.hits.len())
    } else {
        format!(" {total} results ")
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    app.page_rows = inner.height.max(1) as usize;

    if let Some(err) = &app.error {
        frame.render_widget(Paragraph::new(err.as_str()).red().block(block), area);
        return;
    }
    if app.hits.is_empty() {
        let msg = if app.counts.is_empty() && app.query.is_empty() {
            "The catalog is empty. Add a volume with `gfiles volume add <path>`, then run `gfiles scan`."
        } else {
            "No matches."
        };
        frame.render_widget(Paragraph::new(msg).dark_gray().block(block), area);
        return;
    }

    let loc_width = app
        .hits
        .iter()
        .map(|h| h.machine.chars().count() + h.volume.chars().count() + 1)
        .max()
        .unwrap_or(0)
        .clamp(6, 24) as u16;
    // Everything except the path column: the other columns, the gaps between columns, and the
    // highlight marker.
    let fixed =
        KIND_WIDTH + SIZE_WIDTH + loc_width + 3 * COLUMN_GAP + HIGHLIGHT.chars().count() as u16;
    let path_width = inner.width.saturating_sub(fixed) as usize;

    let rows = app.hits.iter().map(|h| {
        Row::new(vec![
            Cell::from(h.kind.as_str()),
            Cell::from(Line::from(fmt::size(h.size)).right_aligned()),
            Cell::from(truncate_left(
                &format!("{}:{}", h.machine, h.volume),
                loc_width as usize,
            ))
            .dark_gray(),
            Cell::from(truncate_left(&h.path, path_width)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(KIND_WIDTH),
            Constraint::Length(SIZE_WIDTH),
            Constraint::Length(loc_width),
            Constraint::Fill(1),
        ],
    )
    .block(block)
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .column_spacing(COLUMN_GAP)
    .highlight_symbol(HIGHLIGHT);
    frame.render_stateful_widget(table, area, &mut app.table);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let lines = match app.selected() {
        Some(h) => vec![
            Line::from(truncate_left(
                &format!("{}:{}", h.machine, h.abs_path),
                area.width.saturating_sub(2) as usize,
            )),
            Line::from(format!(
                "{} · {} · modified {} · volume {}",
                h.kind,
                fmt::size(h.size),
                fmt::date(h.mtime),
                h.volume
            ))
            .dark_gray(),
        ],
        None => vec![],
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Details ")),
        area,
    );
}

/// Shortens `s` to `width` characters by cutting from the left, since the end of a path (the
/// file name) matters most.
fn truncate_left(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let keep: String = s.chars().skip(len - (width - 1)).collect();
    format!("…{keep}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::app_with_files;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_results_counts_and_details() {
        let (_tmp, mut app) =
            app_with_files(&["Books/Tolkien/The Hobbit.epub", "Photos/beach.jpg"]);
        let screen = render(&mut app, 100, 20);
        assert!(screen.contains(" 2 results "), "{screen}");
        assert!(screen.contains("book"), "{screen}");
        assert!(screen.contains("gaffer:home"), "{screen}");
        assert!(screen.contains("Books/Tolkien/The Hobbit.epub"), "{screen}");
        assert!(screen.contains("modified"), "{screen}");
    }

    #[test]
    fn long_paths_keep_their_file_name() {
        let deep = format!("{}/The Hobbit.epub", "very long folder name/".repeat(5));
        let (_tmp, mut app) = app_with_files(&[deep.as_str()]);
        let screen = render(&mut app, 70, 15);
        assert!(screen.contains("…"), "{screen}");
        assert!(screen.contains("The Hobbit.epub"), "{screen}");
    }

    #[test]
    fn empty_catalog_explains_what_to_do() {
        let (_tmp, mut app) = app_with_files(&[]);
        let screen = render(&mut app, 120, 15);
        assert!(screen.contains("The catalog is empty"), "{screen}");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_left("abcdef", 10), "abcdef");
        assert_eq!(truncate_left("abcdef", 4), "…def");
        assert_eq!(truncate_left("abcdef", 0), "");
    }
}
