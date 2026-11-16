//! TUI state and key handling, kept apart from drawing so it can be tested directly.

use std::collections::HashMap;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;
use rusqlite::Connection;

use crate::classify;
use crate::search::{self, Hit, Query};

/// Most rows the results table loads; the title still shows the full match count.
pub const RESULT_LIMIT: usize = 1000;

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Quit,
    /// Enter on a result: its absolute path, printed after the TUI closes.
    Selected(String),
}

pub struct App {
    conn: Connection,
    pub query: String,
    /// Sidebar entries: `"all"` and then every kind.
    pub kinds: Vec<&'static str>,
    pub kind_idx: usize,
    /// Matches per kind for the current words (ignoring the kind filter).
    pub counts: HashMap<String, i64>,
    pub hits: Vec<Hit>,
    pub table: TableState,
    /// Rows visible in the results table, set while drawing; used for PageUp/PageDown.
    pub page_rows: usize,
    pub error: Option<String>,
    pub outcome: Option<Outcome>,
}

impl App {
    pub fn new(conn: Connection) -> Result<App> {
        let mut app = App {
            conn,
            query: String::new(),
            kinds: std::iter::once("all")
                .chain(classify::all_kinds())
                .collect(),
            kind_idx: 0,
            counts: HashMap::new(),
            hits: Vec::new(),
            table: TableState::default(),
            page_rows: 10,
            error: None,
            outcome: None,
        };
        app.refresh();
        Ok(app)
    }

    pub fn kind(&self) -> Option<&'static str> {
        (self.kind_idx > 0).then(|| self.kinds[self.kind_idx])
    }

    /// Entries matching the current words and kind (the count, not just the loaded rows).
    pub fn total(&self) -> i64 {
        match self.kind() {
            Some(k) => self.counts.get(k).copied().unwrap_or(0),
            None => self.counts.values().sum(),
        }
    }

    pub fn count_for(&self, idx: usize) -> i64 {
        match idx {
            0 => self.counts.values().sum(),
            i => self.counts.get(self.kinds[i]).copied().unwrap_or(0),
        }
    }

    pub fn selected(&self) -> Option<&Hit> {
        self.table.selected().and_then(|i| self.hits.get(i))
    }

    fn refresh(&mut self) {
        let query = Query {
            terms: vec![self.query.clone()],
            kind: self.kind().map(String::from),
            limit: Some(RESULT_LIMIT),
            ..Default::default()
        };
        let result = search::count_by_kind(&self.conn, &query).and_then(|counts| {
            self.counts = counts.into_iter().collect();
            search::search(&self.conn, &query)
        });
        match result {
            Ok(hits) => {
                self.hits = hits;
                self.error = None;
            }
            Err(e) => {
                self.hits.clear();
                self.error = Some(format!("{e:#}"));
            }
        }
        self.table
            .select(if self.hits.is_empty() { None } else { Some(0) });
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.outcome = Some(Outcome::Quit),
            KeyCode::Char('u') if ctrl => self.set_query(String::new()),
            KeyCode::Char('n') if ctrl => self.move_by(1),
            KeyCode::Char('p') if ctrl => self.move_by(-1),
            KeyCode::Char(c) if !ctrl => {
                let mut q = self.query.clone();
                q.push(c);
                self.set_query(q);
            }
            KeyCode::Backspace => {
                let mut q = self.query.clone();
                q.pop();
                self.set_query(q);
            }
            KeyCode::Esc if self.query.is_empty() => self.outcome = Some(Outcome::Quit),
            KeyCode::Esc => self.set_query(String::new()),
            KeyCode::Down => self.move_by(1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(self.page_rows as isize),
            KeyCode::PageUp => self.move_by(-(self.page_rows as isize)),
            KeyCode::Tab => self.cycle_kind(1),
            KeyCode::BackTab => self.cycle_kind(-1),
            KeyCode::Enter => {
                if let Some(hit) = self.selected() {
                    self.outcome = Some(Outcome::Selected(hit.abs_path.clone()));
                }
            }
            _ => {}
        }
    }

    fn set_query(&mut self, query: String) {
        if query != self.query {
            self.query = query;
            self.refresh();
        }
    }

    fn cycle_kind(&mut self, step: isize) {
        let n = self.kinds.len() as isize;
        self.kind_idx = (self.kind_idx as isize + step).rem_euclid(n) as usize;
        self.refresh();
    }

    fn move_by(&mut self, step: isize) {
        let Some(current) = self.table.selected() else {
            return;
        };
        let last = self.hits.len().saturating_sub(1) as isize;
        self.table
            .select(Some((current as isize + step).clamp(0, last) as usize));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{config::VolumeConfig, db, scan};
    use ratatui::crossterm::event::KeyEvent;
    use std::fs;

    pub(crate) fn app_with_files(files: &[&str]) -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        for rel in files {
            let p = tmp.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, rel).unwrap();
        }
        let mut conn = db::open_in_memory().unwrap();
        let vol = VolumeConfig {
            id: "v1".into(),
            name: "home".into(),
            path: tmp.path().into(),
            ignore: vec![],
        };
        scan::scan_volume(&mut conn, &vol, "gaffer").unwrap();
        (tmp, App::new(conn).unwrap())
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_str(app: &mut App, s: &str) {
        s.chars().for_each(|c| press(app, KeyCode::Char(c)));
    }

    const FILES: &[&str] = &[
        "Books/Tolkien/The Hobbit.epub",
        "Books/Tolkien/notes.txt",
        "Docs/tolkien-essay.pdf",
        "Photos/beach.jpg",
    ];

    #[test]
    fn typing_filters_and_tab_picks_a_kind() {
        let (_tmp, mut app) = app_with_files(FILES);
        assert_eq!(app.total(), 4);

        type_str(&mut app, "tolk");
        assert_eq!(app.total(), 3);
        assert_eq!(
            app.count_for(app.kinds.iter().position(|k| *k == "book").unwrap()),
            1
        );

        press(&mut app, KeyCode::Tab); // "all" → "book"
        assert_eq!(app.kind(), Some("book"));
        assert_eq!(app.hits.len(), 1);
        assert_eq!(
            app.selected().unwrap().path,
            "Books/Tolkien/The Hobbit.epub"
        );

        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.kind(), None);

        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.query, "tol");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.query, "");
        assert_eq!(app.outcome, None);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.outcome, Some(Outcome::Quit));
    }

    #[test]
    fn moving_is_clamped_and_enter_selects() {
        let (_tmp, mut app) = app_with_files(FILES);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.table.selected(), Some(0));
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.table.selected(), Some(3));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.table.selected(), Some(3));

        press(&mut app, KeyCode::Enter);
        let Some(Outcome::Selected(path)) = &app.outcome else {
            panic!("expected a selection")
        };
        assert!(path.ends_with("Photos/beach.jpg"));
    }

    #[test]
    fn enter_with_no_results_does_nothing() {
        let (_tmp, mut app) = app_with_files(FILES);
        type_str(&mut app, "zzz");
        assert!(app.hits.is_empty());
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.outcome, None);
    }
}
