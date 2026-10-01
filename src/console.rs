//! A read-only SQL console: type a query, see the rows.

use crate::diagram::{truncate, HL_COLOR};
use crate::schema::{self, QueryResult};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use std::time::Instant;

const ROW_LIMIT: usize = 1000;

/// Rows as an aligned text grid (also used by the table preview).
pub fn grid_lines(headers: &[String], rows: &[Vec<String>], header_color: impl Fn(&str) -> Color) -> Vec<Line<'static>> {
    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain(std::iter::once(headers[i].chars().count()))
                .max()
                .unwrap_or(0)
                .min(40)
        })
        .collect();
    let cell = |s: &str, w: usize| format!("{:<w$}", truncate(&s.replace(['\n', '\r', '\t'], " "), w), w = w);
    let sep = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(Line::from(
        headers
            .iter()
            .enumerate()
            .flat_map(|(i, h)| {
                [
                    Span::styled(cell(h, widths[i]), Style::default().fg(header_color(h)).add_modifier(Modifier::BOLD)),
                    Span::styled(" │ ", sep),
                ]
            })
            .collect::<Vec<_>>(),
    ));
    lines.push(Line::styled(widths.iter().map(|&w| "─".repeat(w)).collect::<Vec<_>>().join("─┼─"), sep));
    for row in rows {
        lines.push(Line::from(
            row.iter()
                .enumerate()
                .flat_map(|(i, v)| {
                    let st = if v == "NULL" { Style::default().fg(Color::DarkGray) } else { Style::default() };
                    [Span::styled(cell(v, widths[i]), st), Span::styled(" │ ", sep)]
                })
                .collect::<Vec<_>>(),
        ));
    }
    lines
}

pub struct Console {
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    hist: Option<usize>,
    result: Option<QueryResult>,
    lines: Vec<Line<'static>>,
    error: Option<String>,
    elapsed_ms: u128,
    sx: usize,
    sy: usize,
}

impl Console {
    pub fn new() -> Self {
        Console { input: Vec::new(), cursor: 0, history: Vec::new(), hist: None, result: None, lines: Vec::new(), error: None, elapsed_ms: 0, sx: 0, sy: 0 }
    }

    /// Nothing typed or run yet.
    pub fn is_fresh(&self) -> bool {
        self.input.is_empty() && self.history.is_empty()
    }

    pub fn set_query(&mut self, q: &str) {
        self.input = q.chars().collect();
        self.cursor = self.input.len();
    }

    pub fn scroll(&mut self, dy: i64, dx: i64) {
        self.sy = (self.sy as i64 + dy).max(0) as usize;
        self.sx = (self.sx as i64 + dx).max(0) as usize;
    }

    pub fn run(&mut self, path: &str) {
        let q: String = self.input.iter().collect();
        if !q.trim().is_empty() && self.history.last() != Some(&q) {
            self.history.push(q.clone());
        }
        self.hist = None;
        let t = Instant::now();
        match schema::run_query(path, &q, ROW_LIMIT) {
            Ok(r) => {
                self.elapsed_ms = t.elapsed().as_millis();
                self.lines = grid_lines(&r.headers, &r.rows, |_| Color::White);
                self.result = Some(r);
                self.error = None;
                (self.sx, self.sy) = (0, 0);
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Returns true when the console should close.
    pub fn on_key(&mut self, k: KeyEvent, path: &str) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.input.len();
        match k.code {
            KeyCode::Esc => return true,
            KeyCode::Enter => self.run(path),
            KeyCode::Up if !self.history.is_empty() => {
                let i = match self.hist {
                    None => self.history.len() - 1,
                    Some(i) => i.saturating_sub(1),
                };
                self.hist = Some(i);
                let q = self.history[i].clone();
                self.set_query(&q);
            }
            KeyCode::Down => {
                if let Some(i) = self.hist {
                    if i + 1 < self.history.len() {
                        self.hist = Some(i + 1);
                        let q = self.history[i + 1].clone();
                        self.set_query(&q);
                    } else {
                        self.hist = None;
                        self.set_query("");
                    }
                }
            }
            KeyCode::Left if ctrl => self.scroll(0, -8),
            KeyCode::Right if ctrl => self.scroll(0, 8),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::PageUp => self.scroll(-10, 0),
            KeyCode::PageDown => self.scroll(10, 0),
            KeyCode::Char('u') if ctrl => self.set_query(""),
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < len => {
                self.input.remove(self.cursor);
            }
            _ => {}
        }
        false
    }

    pub fn on_paste(&mut self, text: &str) {
        for ch in text.chars().map(|c| if c.is_control() { ' ' } else { c }) {
            self.input.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        if area.height < 4 || area.width < 20 {
            return;
        }
        let dim = Style::default().fg(Color::DarkGray);
        // input line, scrolled so the cursor stays visible
        let prompt = "sql> ";
        let room = (area.width as usize).saturating_sub(prompt.len() + 1).max(1);
        let start = self.cursor.saturating_sub(room);
        let shown: String = self.input.iter().skip(start).take(room).collect();
        f.render_widget(
            Line::from(vec![Span::styled(prompt, Style::default().fg(HL_COLOR).add_modifier(Modifier::BOLD)), Span::raw(shown)]),
            Rect::new(area.x, area.y, area.width, 1),
        );
        f.set_cursor_position(Position::new(area.x + (prompt.len() + self.cursor - start) as u16, area.y));

        let status = match (&self.error, &self.result) {
            (Some(e), _) => Line::styled(format!("✗ {e}"), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            (None, Some(r)) => Line::styled(
                format!(
                    "{} row{}{} · {} ms",
                    r.rows.len(),
                    if r.rows.len() == 1 { "" } else { "s" },
                    if r.truncated { format!(" (first {ROW_LIMIT})") } else { String::new() },
                    self.elapsed_ms
                ),
                Style::default().fg(Color::Green),
            ),
            (None, None) => Line::styled("read-only: queries can look at the data, not change it", dim),
        };
        f.render_widget(status, Rect::new(area.x, area.y + 1, area.width, 1));

        let block = Block::new().borders(Borders::TOP).border_style(dim).title(" results ").title_bottom(Line::styled(
            " enter run · ↑↓ history · pgup/pgdn rows · ctrl+←/→ columns · ctrl+u clear · esc close ",
            dim,
        ));
        let res = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
        let inner_h = block.inner(res).height as usize;
        let widest = self.lines.iter().map(|l| l.width()).max().unwrap_or(0);
        self.sy = self.sy.min(self.lines.len().saturating_sub(inner_h));
        self.sx = self.sx.min(widest.saturating_sub(area.width as usize));
        f.render_widget(Paragraph::new(self.lines.clone()).block(block).scroll((self.sy as u16, self.sx as u16)), res);
    }
}
