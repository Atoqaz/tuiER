use crate::diagram::{self, wrap, Canvas, Grid, GridMap, Measure, Selection, FK_COLOR, HL_COLOR, MOVE_COLOR, PK_COLOR};
use crate::console::{self, Console};
use crate::designer::{Change, Draft, Outcome};
use crate::lint::{self, Issue, Severity};
use crate::export::{ExportCtx, Format};
use crate::schema::{self, ColRef, Schema};
use anyhow::{bail, Result};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
    DefaultTerminal, Frame,
};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

const PREVIEW_ROWS: usize = 50;
const UNDO_LIMIT: usize = 200;

pub struct Config {
    pub db_path: String,
    pub db_name: String,
    pub desc_table: String,
    pub desc_width: usize,
    pub read_only: bool,
    /// send OSC 22 pointer shapes (ignored by terminals without support)
    pub pointer_shapes: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pointer {
    Default,
    Hand,
    Grab,
    Grabbing,
}

impl Pointer {
    fn css_name(self) -> &'static str {
        match self {
            Pointer::Default => "default",
            Pointer::Hand => "pointer",
            Pointer::Grab => "grab",
            Pointer::Grabbing => "grabbing",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    NewTable,
    Query,
    Lint,
    Undo,
    Redo,
    Reset,
    Help,
}

/// A table being dragged with the mouse. The diagram shows the result live;
/// `before` is restored on Esc, and pushed on the undo stack on drop.
struct Drag {
    table: usize,
    before: Grid,
    /// cell geometry from before the drag started, so the target cell
    /// doesn't jump around while the preview reshapes the diagram
    map: GridMap,
    target: (usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

struct Press {
    x: u16,
    y: u16,
    sx: usize,
    sy: usize,
    moved: bool,
    /// dragging a table by its title instead of panning
    drag_table: Option<usize>,
}

/// Single-paragraph text editor for a description.
struct Editor {
    target: Selection,
    text: Vec<char>,
    cursor: usize,
}

struct Search {
    query: String,
    prev_sel: Option<Selection>,
    prev_scroll: (usize, usize),
    matches: Vec<Selection>,
    idx: usize,
}

struct Popup {
    title: String,
    lines: Vec<Line<'static>>,
    sx: usize,
    sy: usize,
}

enum Mode {
    Normal,
    Edit(Editor),
    Search(Search),
    Popup(Popup),
    Export,
    Design(Box<Draft>),
    Menu(Menu),
    Confirm(Confirm),
    Prompt(Prompt),
    Lint(LintView),
    Console,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum MenuAction {
    Edit,
    Colors,
    SetColor(Option<&'static str>),
    Duplicate { with_rows: bool },
    Delete,
    Preview,
    Info,
    Query,
    Focus,
    Collapse,
}

struct MenuItem {
    key: char,
    label: String,
    action: MenuAction,
    swatch: Option<Color>,
}

/// The table menu (m or right-click), and its colour submenu.
struct Menu {
    table: usize,
    title: String,
    items: Vec<MenuItem>,
    idx: usize,
}

enum ConfirmAction {
    DeleteTable(usize),
}

struct Confirm {
    lines: Vec<String>,
    action: ConfirmAction,
}

enum PromptAction {
    Duplicate { table: usize, with_rows: bool },
}

/// A one-line question in the footer.
struct Prompt {
    label: String,
    text: String,
    action: PromptAction,
}

struct LintView {
    issues: Vec<Issue>,
    idx: usize,
}

type Stamp = Option<(std::time::SystemTime, u64, Option<std::time::SystemTime>)>;

pub struct App {
    schema: Schema,
    cfg: Config,
    auto_grid: Grid,
    /// layout of all tables (automatic, or arranged by the user)
    base_grid: Grid,
    /// what is drawn: base_grid, or a subset of it in focus mode
    grid: Grid,
    measures: Vec<Measure>,
    sel: Option<Selection>,
    history: Vec<Selection>,
    /// focus mode: centre table (its neighbours are shown too)
    focus: Option<usize>,
    collapsed: HashSet<usize>,
    canvas: Canvas,
    sx: usize,
    sy: usize,
    view: Rect,
    press: Option<Press>,
    /// shown tables in reading order (grid column, then row)
    order: Vec<usize>,
    mode: Mode,
    /// (message, is_error) shown in the footer until the next key press
    status: Option<(String, bool)>,
    edit_w: usize,
    /// cycling through the columns referencing a PK: (pk, index)
    follow: Option<(ColRef, usize)>,
    drag: Option<Drag>,
    undo: Vec<Grid>,
    redo: Vec<Grid>,
    /// toolbar buttons as drawn in the last frame
    buttons: Vec<(Rect, Button)>,
    hover_button: Option<Button>,
    /// table whose title is under the mouse (drag handle feedback)
    hover_title: Option<usize>,
    pointer: Pointer,
    pending_pointer: Option<Pointer>,
    /// chosen table colours, by lowercase table name
    colors: HashMap<String, String>,
    /// (errors, warnings) found by the lint checks
    lint_counts: (usize, usize),
    console: Console,
    /// to notice the database changing on disk
    file_stamp: Stamp,
    last_disk_check: std::time::Instant,
    pending_reload: bool,
    quit: bool,
}

/// `g` with table `t` moved to cell (c, r); a table already there swaps places.
fn moved_grid(g: &Grid, t: usize, c: usize, r: usize) -> Grid {
    let Some(from) = g.pos[t] else { return g.clone() };
    let (c, r) = (c.min(g.ncols), r.min(g.nrows));
    if from == (c, r) {
        return g.clone();
    }
    let mut pos = g.pos.clone();
    if let Some(u) = g.occupant(c, r) {
        pos[u] = Some(from);
    }
    pos[t] = Some((c, r));
    Grid::from_positions(pos).compacted(|_| true)
}

fn table_of(sel: Selection) -> usize {
    match sel {
        Selection::Table(t) => t,
        Selection::Column(c) => c.table,
    }
}

impl App {
    pub fn new(schema: Schema, cfg: Config) -> Self {
        let auto_grid = diagram::compute_grid(&schema);
        let mut status = None;
        let base_grid = match schema::load_layout(&cfg.db_path) {
            Ok(saved) if !saved.is_empty() => diagram::with_saved(&schema, &auto_grid, &saved),
            Ok(_) => auto_grid.clone(),
            Err(e) => {
                status = Some((format!("could not read saved layout: {e:#}"), true));
                auto_grid.clone()
            }
        };
        let measures: Vec<Measure> = schema.tables.iter().map(|t| diagram::measure(t, cfg.desc_width)).collect();
        let collapsed = HashSet::new();
        let canvas = diagram::build(&schema, &base_grid, &measures, None, &collapsed, None, &[]);
        let mut app = App {
            schema,
            cfg,
            grid: base_grid.clone(),
            auto_grid,
            base_grid,
            measures,
            sel: None,
            history: Vec::new(),
            focus: None,
            collapsed,
            canvas,
            sx: 0,
            sy: 0,
            view: Rect::default(),
            press: None,
            order: Vec::new(),
            mode: Mode::Normal,
            status,
            edit_w: 40,
            follow: None,
            drag: None,
            undo: Vec::new(),
            redo: Vec::new(),
            buttons: Vec::new(),
            hover_button: None,
            hover_title: None,
            pointer: Pointer::Default,
            pending_pointer: None,
            colors: HashMap::new(),
            lint_counts: (0, 0),
            console: Console::new(),
            file_stamp: None,
            last_disk_check: std::time::Instant::now(),
            pending_reload: false,
            quit: false,
        };
        app.colors = schema::load_colors(&app.cfg.db_path).unwrap_or_default();
        app.lint_counts = lint::counts(&lint::run(&app.schema, &app.cfg.db_path));
        app.touch_stamp();
        app.rebuild();
        app
    }

    pub fn warnings(&self) -> &[String] {
        &self.schema.warnings
    }

    pub fn canvas_text(&self) -> String {
        self.canvas.to_text()
    }

    /// "table" or "table.column"
    pub fn resolve(&self, spec: &str) -> Result<Selection> {
        let (t, c) = match spec.split_once('.') {
            Some((t, c)) => (t, Some(c)),
            None => (spec, None),
        };
        let Some(ti) = self.schema.find_table(t) else { bail!("table '{t}' not found") };
        match c {
            None => Ok(Selection::Table(ti)),
            Some(c) => match self.schema.tables[ti].columns.iter().position(|x| x.name.eq_ignore_ascii_case(c)) {
                Some(ci) => Ok(Selection::Column(ColRef { table: ti, column: ci })),
                None => bail!("column '{c}' not found in '{t}'"),
            },
        }
    }

    fn rebuild(&mut self) {
        self.grid = match self.focus {
            Some(t) => {
                let keep = self.neighbours(t);
                self.base_grid.compacted(|i| keep.contains(&i))
            }
            None => self.base_grid.clone(),
        };
        let moving = self.drag.as_ref().map(|d| d.table);
        let colors: Vec<Option<Color>> = self
            .schema
            .tables
            .iter()
            .map(|t| self.colors.get(&t.name.to_lowercase()).and_then(|c| diagram::color_named(c)))
            .collect();
        self.canvas = diagram::build(&self.schema, &self.grid, &self.measures, self.sel, &self.collapsed, moving, &colors);
        let mut order: Vec<usize> = (0..self.schema.tables.len()).filter(|&t| self.grid.pos[t].is_some()).collect();
        order.sort_by_key(|&t| self.grid.pos[t]);
        self.order = order;
        self.clamp();
    }

    fn neighbours(&self, t: usize) -> HashSet<usize> {
        let mut s = HashSet::from([t]);
        for fk in &self.schema.fks {
            if fk.from.table == t {
                s.insert(fk.to.table);
            }
            if fk.to.table == t {
                s.insert(fk.from.table);
            }
        }
        s
    }

    pub fn select(&mut self, sel: Option<Selection>) {
        self.sel = sel;
        if let (Some(_), Some(s)) = (self.focus, sel) {
            self.focus = Some(table_of(s)); // focus walks with the selection
        }
        self.rebuild();
    }

    /// Select and remember where we came from (for `b`).
    fn jump(&mut self, to: Selection) {
        if let Some(cur) = self.sel {
            if cur != to {
                self.history.push(cur);
                if self.history.len() > 200 {
                    self.history.remove(0);
                }
            }
        }
        self.select(Some(to));
        self.reveal(self.focus.is_some());
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), error));
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if let Some(p) = self.pending_pointer.take() {
                if self.cfg.pointer_shapes {
                    let mut out = std::io::stdout();
                    let _ = write!(out, "\x1b]22;{}\x1b\\", p.css_name());
                    let _ = out.flush();
                }
            }
            if event::poll(std::time::Duration::from_millis(500))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
                    Event::Mouse(m) => self.on_mouse(m),
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
            if self.last_disk_check.elapsed() >= std::time::Duration::from_millis(500) {
                self.check_disk();
            }
        }
        if self.cfg.pointer_shapes {
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]22;\x1b\\"); // back to the terminal's default
            let _ = out.flush();
        }
        Ok(())
    }

    // ================================================================ input

    fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.drag.is_some() {
            if k.code == KeyCode::Esc {
                self.cancel_drag();
            }
            return; // keys are ignored mid-drag
        }
        match self.mode {
            Mode::Edit(_) => self.on_edit_key(k),
            Mode::Search(_) => self.on_search_key(k),
            Mode::Popup(_) => self.on_popup_key(k),
            Mode::Export => self.on_export_key(k),
            Mode::Design(_) => self.on_design_key(k),
            Mode::Menu(_) => self.on_menu_key(k),
            Mode::Confirm(_) => self.on_confirm_key(k),
            Mode::Prompt(_) => self.on_prompt_key(k),
            Mode::Lint(_) => self.on_lint_key(k),
            Mode::Console => {
                if self.console.on_key(k, &self.cfg.db_path) {
                    self.mode = Mode::Normal;
                }
            }
            Mode::Normal => {
                self.status = None;
                self.on_normal_key(k);
            }
        }
        self.clamp();
    }

    fn on_normal_key(&mut self, k: KeyEvent) {
        let (vw, vh) = (self.view.width as usize, self.view.height as usize);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.select(None),
            KeyCode::Char('?') => self.show_help(),
            // selection
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            // arrange: shift+arrows (before the plain arrows below)
            KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => self.nudge(Dir::Up),
            KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => self.nudge(Dir::Down),
            KeyCode::Left if k.modifiers.contains(KeyModifiers::SHIFT) => self.nudge(Dir::Left),
            KeyCode::Right if k.modifiers.contains(KeyModifiers::SHIFT) => self.nudge(Dir::Right),
            KeyCode::Up => self.nav(Dir::Up),
            KeyCode::Down => self.nav(Dir::Down),
            KeyCode::Left => self.nav(Dir::Left),
            KeyCode::Right => self.nav(Dir::Right),
            KeyCode::Char('f') => self.follow(),
            KeyCode::Char('b') => self.back(),
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('n') => self.next_missing(1),
            KeyCode::Char('N') => self.next_missing(-1),
            // arrange
            KeyCode::Char('W') => self.nudge(Dir::Up),
            KeyCode::Char('S') => self.nudge(Dir::Down),
            KeyCode::Char('A') => self.nudge(Dir::Left),
            KeyCode::Char('D') => self.nudge(Dir::Right),
            KeyCode::Char('R') => self.reset_layout(),
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('U') => self.redo(),
            KeyCode::Char('z') if k.modifiers.contains(KeyModifiers::CONTROL) => self.undo(),
            KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::CONTROL) => self.redo(),
            KeyCode::Char('z') => self.toggle_focus(),
            KeyCode::Char(' ') => self.toggle_collapse(),
            KeyCode::Char('-') => {
                self.collapsed = (0..self.schema.tables.len()).collect();
                self.rebuild();
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.collapsed.clear();
                self.rebuild();
            }
            // actions
            KeyCode::Char('e') | KeyCode::Char('E') => self.edit_selected(),
            KeyCode::Char('p') => self.show_preview(),
            KeyCode::Char('i') => self.show_info(),
            KeyCode::Char('x') => self.mode = Mode::Export,
            KeyCode::Char('t') => self.start_design(),
            KeyCode::Char('m') => self.open_menu(),
            KeyCode::Char('!') => self.open_lint(),
            KeyCode::Char(':') => self.open_console(None),
            KeyCode::Delete => {
                if let Some(t) = self.sel.map(table_of) {
                    self.confirm_delete(t);
                }
            }
            // viewport
            KeyCode::Char('c') | KeyCode::Enter => self.reveal(true),
            KeyCode::Char('a') | KeyCode::Char('h') => self.sx = self.sx.saturating_sub(4),
            KeyCode::Char('d') | KeyCode::Char('l') => self.sx += 4,
            KeyCode::Char('w') | KeyCode::Char('k') => self.sy = self.sy.saturating_sub(2),
            KeyCode::Char('s') | KeyCode::Char('j') => self.sy += 2,
            KeyCode::Char('H') => self.sx = self.sx.saturating_sub(vw / 2),
            KeyCode::Char('L') => self.sx += vw / 2,
            KeyCode::PageUp => self.sy = self.sy.saturating_sub(vh.max(1)),
            KeyCode::PageDown => self.sy += vh.max(1),
            KeyCode::Home | KeyCode::Char('g') => (self.sx, self.sy) = (0, 0),
            KeyCode::End | KeyCode::Char('G') => (self.sx, self.sy) = (usize::MAX, usize::MAX),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let shift = m.modifiers.contains(KeyModifiers::SHIFT);
        if let Mode::Popup(p) = &mut self.mode {
            match m.kind {
                MouseEventKind::ScrollDown if shift => p.sx += 6,
                MouseEventKind::ScrollUp if shift => p.sx = p.sx.saturating_sub(6),
                MouseEventKind::ScrollDown => p.sy += 3,
                MouseEventKind::ScrollUp => p.sy = p.sy.saturating_sub(3),
                MouseEventKind::Down(_) => self.mode = Mode::Normal,
                _ => {}
            }
            return;
        }
        if let Mode::Design(d) = &mut self.mode {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => d.on_click(m.column, m.row),
                MouseEventKind::Moved => d.on_hover(m.column, m.row),
                _ => {}
            }
            return;
        }
        match &mut self.mode {
            Mode::Console => {
                match m.kind {
                    MouseEventKind::ScrollDown if shift => self.console.scroll(0, 6),
                    MouseEventKind::ScrollUp if shift => self.console.scroll(0, -6),
                    MouseEventKind::ScrollDown => self.console.scroll(3, 0),
                    MouseEventKind::ScrollUp => self.console.scroll(-3, 0),
                    _ => {}
                }
                return;
            }
            Mode::Menu(_) | Mode::Confirm(_) | Mode::Lint(_) => {
                if let MouseEventKind::Down(_) = m.kind {
                    self.mode = Mode::Normal; // a click outside closes it
                }
                return;
            }
            _ => {}
        }
        // right-click on a table: its menu
        if let (MouseEventKind::Down(MouseButton::Right), Mode::Normal) = (m.kind, &self.mode) {
            if let Some(hit) = self.canvas_point(m.column, m.row).and_then(|(x, y)| self.canvas.hit(x, y)) {
                if self.sel.map(table_of) != Some(table_of(hit)) {
                    self.select(Some(hit));
                }
                self.open_menu();
            }
            return;
        }
        let normal = matches!(self.mode, Mode::Normal);
        match m.kind {
            MouseEventKind::Moved => self.hover(m.column, m.row),
            MouseEventKind::Down(MouseButton::Left) => {
                let drag_table = match self.canvas_point(m.column, m.row).and_then(|(x, y)| self.canvas.hit(x, y)) {
                    Some(Selection::Table(t)) if normal && self.focus.is_none() => Some(t),
                    _ => None,
                };
                self.press = Some(Press { x: m.column, y: m.row, sx: self.sx, sy: self.sy, moved: false, drag_table });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let mut drag_table = None;
                if let Some(p) = &mut self.press {
                    let dx = m.column as i64 - p.x as i64;
                    let dy = m.row as i64 - p.y as i64;
                    if dx != 0 || dy != 0 {
                        p.moved = true;
                    }
                    match p.drag_table {
                        Some(t) if p.moved => drag_table = Some(t),
                        Some(_) => {}
                        None => {
                            self.sx = (p.sx as i64 - dx).max(0) as usize;
                            self.sy = (p.sy as i64 - dy).max(0) as usize;
                        }
                    }
                }
                if let Some(t) = drag_table {
                    if self.drag.is_none() {
                        self.start_drag(t);
                    }
                    self.update_drag(m.column, m.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let press = self.press.take();
                if self.drag.is_some() {
                    self.finish_drag();
                    self.hover(m.column, m.row);
                } else if let Some(p) = press {
                    if !p.moved && normal {
                        match self.button_at(m.column, m.row) {
                            Some(b) => self.press_button(b),
                            None => self.click(m.column, m.row),
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown if shift => self.sx += 6,
            MouseEventKind::ScrollUp if shift => self.sx = self.sx.saturating_sub(6),
            MouseEventKind::ScrollDown => self.sy += 3,
            MouseEventKind::ScrollUp => self.sy = self.sy.saturating_sub(3),
            MouseEventKind::ScrollRight => self.sx += 6,
            MouseEventKind::ScrollLeft => self.sx = self.sx.saturating_sub(6),
            _ => {}
        }
        self.clamp();
    }

    fn on_paste(&mut self, text: &str) {
        let clean = text.chars().map(|c| if c.is_control() { ' ' } else { c });
        match &mut self.mode {
            Mode::Edit(ed) => {
                for ch in clean {
                    ed.text.insert(ed.cursor, ch);
                    ed.cursor += 1;
                }
            }
            Mode::Search(s) => {
                s.query.extend(clean);
                self.update_search();
            }
            Mode::Design(d) => d.on_paste(text),
            Mode::Console => self.console.on_paste(text),
            Mode::Prompt(p) => p.text.push_str(&text.replace(['\n', '\r'], " ")),
            _ => {}
        }
    }

    fn canvas_point(&self, mx: u16, my: u16) -> Option<(usize, usize)> {
        self.view
            .contains(Position::new(mx, my))
            .then(|| (self.sx + (mx - self.view.x) as usize, self.sy + (my - self.view.y) as usize))
    }

    fn click(&mut self, mx: u16, my: u16) {
        let Some((x, y)) = self.canvas_point(mx, my) else { return };
        let hit = self.canvas.hit(x, y);
        // clicking the current selection again collapses it
        self.select(if hit == self.sel { None } else { hit });
        if self.focus.is_some() && self.sel.is_some() {
            self.reveal(true); // focus layout changed around the new table
        }
    }

    // =========================================================== navigation

    fn shown_cols(&self, t: usize) -> Vec<usize> {
        (0..self.schema.tables[t].columns.len())
            .filter(|&c| self.canvas.col_y.contains_key(&ColRef { table: t, column: c }))
            .collect()
    }

    fn cycle(&mut self, dir: i64) {
        let items: Vec<ColRef> = self
            .order
            .iter()
            .flat_map(|&t| self.shown_cols(t).into_iter().map(move |c| ColRef { table: t, column: c }))
            .collect();
        if items.is_empty() {
            return;
        }
        let len = items.len() as i64;
        let next = match self.sel {
            Some(Selection::Column(c)) => match items.iter().position(|&x| x == c) {
                Some(i) => (i as i64 + dir).rem_euclid(len),
                None => 0,
            },
            Some(Selection::Table(t)) => items.iter().position(|x| x.table == t).unwrap_or(0) as i64,
            None if dir > 0 => 0,
            None => len - 1,
        };
        self.select(Some(Selection::Column(items[next as usize])));
        self.reveal(false);
    }

    fn sel_y(&self, sel: Selection) -> usize {
        match sel {
            Selection::Column(c) => self.canvas.col_y.get(&c).copied().unwrap_or(0),
            Selection::Table(t) => self.canvas.table_rect[t].map(|r| r.1).unwrap_or(0),
        }
    }

    /// Arrow keys: ↑/↓ step through the rows of a table (title → columns),
    /// and continue into the table above/below; ←/→ jump to the table on
    /// the left/right, landing on the row closest to the current one.
    fn nav(&mut self, d: Dir) {
        let Some(sel) = self.sel else {
            if let Some(&t) = self.order.first() {
                self.select(Some(Selection::Table(t)));
                self.reveal(false);
            }
            return;
        };
        let t = table_of(sel);
        let cols = self.shown_cols(t);
        let col = |c: usize| Selection::Column(ColRef { table: t, column: c });
        let next = match (d, sel) {
            (Dir::Up, Selection::Column(c)) => match cols.iter().position(|&x| x == c.column) {
                Some(i) if i > 0 => Some(col(cols[i - 1])),
                _ => Some(Selection::Table(t)),
            },
            (Dir::Up, Selection::Table(_)) => self.neighbour(t, Dir::Up, None).map(|u| {
                match self.shown_cols(u).last() {
                    Some(&c) => Selection::Column(ColRef { table: u, column: c }),
                    None => Selection::Table(u),
                }
            }),
            (Dir::Down, Selection::Table(_)) => match cols.first() {
                Some(&c) => Some(col(c)),
                None => self.neighbour(t, Dir::Down, None).map(Selection::Table),
            },
            (Dir::Down, Selection::Column(c)) => match cols.iter().position(|&x| x == c.column) {
                Some(i) if i + 1 < cols.len() => Some(col(cols[i + 1])),
                _ => self.neighbour(t, Dir::Down, None).map(Selection::Table),
            },
            (_, _) => {
                let y = self.sel_y(sel);
                self.neighbour(t, d, Some(y)).map(|u| match sel {
                    Selection::Table(_) => Selection::Table(u),
                    Selection::Column(_) => self.closest_col(u, y),
                })
            }
        };
        if let Some(n) = next {
            self.select(Some(n));
            self.reveal(self.focus.is_some());
        }
    }

    fn closest_col(&self, t: usize, y: usize) -> Selection {
        self.shown_cols(t)
            .into_iter()
            .map(|c| ColRef { table: t, column: c })
            .min_by_key(|c| self.canvas.col_y[c].abs_diff(y))
            .map(Selection::Column)
            .unwrap_or(Selection::Table(t))
    }

    /// Nearest table in a direction, preferring ones that line up.
    fn neighbour(&self, t: usize, d: Dir, y_ref: Option<usize>) -> Option<usize> {
        let (x, y, w, h) = self.canvas.table_rect[t]?;
        let cy = y_ref.unwrap_or(y + h / 2);
        let dist = |v: usize, a: usize, b: usize| if v < a { a - v } else if v >= b { v + 1 - b } else { 0 };
        let range_gap = |a0: usize, a1: usize, b0: usize, b1: usize| {
            if a1 <= b0 {
                b0 - a1
            } else {
                a0.saturating_sub(b1)
            }
        };
        self.order
            .iter()
            .filter(|&&u| u != t)
            .filter_map(|&u| {
                let (ux, uy, uw, uh) = self.canvas.table_rect[u]?;
                let score = match d {
                    Dir::Left if ux + uw <= x => (x - (ux + uw)) + 3 * dist(cy, uy, uy + uh),
                    Dir::Right if ux >= x + w => (ux - (x + w)) + 3 * dist(cy, uy, uy + uh),
                    Dir::Up if uy + uh <= y => (y - (uy + uh)) + 4 * range_gap(x, x + w, ux, ux + uw),
                    Dir::Down if uy >= y + h => (uy - (y + h)) + 4 * range_gap(x, x + w, ux, ux + uw),
                    _ => return None,
                };
                Some((score, u))
            })
            .min()
            .map(|(_, u)| u)
    }

    fn referencers(&self, c: ColRef) -> Vec<ColRef> {
        self.schema.fks.iter().filter(|f| f.to == c).map(|f| f.from).collect()
    }

    /// f: FK → referenced column; PK → first referencing column, and
    /// pressing f again there cycles through the other referencing columns.
    fn follow(&mut self) {
        let Some(Selection::Column(c)) = self.sel else {
            self.set_status("select a column first", true);
            return;
        };
        if let Some((pk, i)) = self.follow {
            let by = self.referencers(pk);
            if by.len() > 1 && by.get(i) == Some(&c) {
                let j = (i + 1) % by.len();
                self.follow = Some((pk, j));
                self.jump(Selection::Column(by[j]));
                self.set_status(format!("referencing column {}/{} · f next · b back", j + 1, by.len()), false);
                return;
            }
        }
        if let Some(fk) = self.schema.fks.iter().find(|f| f.from == c) {
            let to = fk.to;
            self.follow = None;
            self.jump(Selection::Column(to));
            return;
        }
        let by = self.referencers(c);
        if by.is_empty() {
            self.set_status("this column has no relations", true);
            return;
        }
        self.follow = Some((c, 0));
        self.jump(Selection::Column(by[0]));
        if by.len() > 1 {
            self.set_status(format!("referencing column 1/{} · f next · b back", by.len()), false);
        }
    }

    fn back(&mut self) {
        match self.history.pop() {
            Some(prev) => {
                self.select(Some(prev));
                self.reveal(self.focus.is_some());
            }
            None => self.set_status("no earlier selection", true),
        }
    }

    /// Scroll so the selection is visible (or centred when `center`).
    fn reveal(&mut self, center: bool) {
        let (vw, vh) = (self.view.width as usize, self.view.height as usize);
        if vw == 0 || vh == 0 {
            return;
        }
        let rect = match self.sel {
            None => return,
            Some(Selection::Table(t)) => self.canvas.table_rect[t],
            Some(Selection::Column(c)) => self.canvas.table_rect[c.table]
                .and_then(|(x, _, w, _)| self.canvas.col_y.get(&c).map(|&y| (x, y, w, 1))),
        };
        let Some((x, y, w, h)) = rect else { return };
        if center || x < self.sx || x + w > self.sx + vw {
            self.sx = if w <= vw { (x + w / 2).saturating_sub(vw / 2) } else { x.saturating_sub(1) };
        }
        if center || y < self.sy || y + h > self.sy + vh {
            self.sy = (y + h / 2).saturating_sub(vh / 2);
        }
        self.clamp();
    }

    fn clamp(&mut self) {
        self.sx = self.sx.min(self.canvas.w.saturating_sub(self.view.width as usize));
        self.sy = self.sy.min(self.canvas.h.saturating_sub(self.view.height as usize));
    }

    // ============================================================ arranging

    fn toggle_focus(&mut self) {
        if self.focus.take().is_none() {
            let Some(sel) = self.sel else {
                self.set_status("select a table first", true);
                return;
            };
            self.focus = Some(table_of(sel));
        }
        self.rebuild();
        self.reveal(true);
    }

    fn toggle_collapse(&mut self) {
        let Some(sel) = self.sel else {
            self.set_status("select a table first ( - / + collapse / expand all )", true);
            return;
        };
        let t = table_of(sel);
        if !self.collapsed.remove(&t) {
            self.collapsed.insert(t);
            if let Selection::Column(c) = sel {
                if !diagram::is_key_column(self.schema.col(c)) {
                    self.sel = Some(Selection::Table(t)); // its row is about to disappear
                }
            }
        }
        self.rebuild();
    }

    fn nudge(&mut self, d: Dir) {
        let Some(sel) = self.sel else {
            self.set_status("select a table first", true);
            return;
        };
        let t = table_of(sel);
        let Some((c, r)) = self.base_grid.pos[t] else { return };
        let target = match d {
            Dir::Left => c.checked_sub(1).map(|c| (c, r)),
            Dir::Right => Some((c + 1, r)),
            Dir::Up => r.checked_sub(1).map(|r| (c, r)),
            Dir::Down => Some((c, r + 1)),
        };
        if let Some((c, r)) = target {
            self.move_table(t, c, r);
        }
    }

    /// Move a table to a grid cell (swapping with its occupant) and save.
    fn move_table(&mut self, t: usize, c: usize, r: usize) {
        if self.focus.is_some() {
            self.set_status("leave focus mode (z) to rearrange tables", true);
            return;
        }
        let before = self.base_grid.clone();
        self.base_grid = moved_grid(&before, t, c, r);
        self.commit_layout(before, "moved");
        self.reveal(false);
    }

    /// Record a layout change for undo and save it.
    fn commit_layout(&mut self, before: Grid, what: &str) {
        self.rebuild();
        if self.base_grid == before {
            return;
        }
        self.undo.push(before);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.persist_layout(what);
    }

    /// Save the current layout (or drop the saved one if it's the automatic one).
    fn persist_layout(&mut self, what: &str) {
        let hint = "u undo";
        if self.cfg.read_only {
            self.set_status(format!("{what} (read-only: not saved) · {hint}"), false);
            return;
        }
        let result = if self.base_grid == self.auto_grid {
            schema::clear_layout(&self.cfg.db_path)
        } else {
            schema::save_layout(&self.cfg.db_path, &self.layout_entries())
        };
        self.touch_stamp();
        match result {
            Ok(()) => self.set_status(format!("{what} · layout saved · {hint}"), false),
            Err(e) => self.set_status(format!("{what}, but saving the layout failed: {e:#}"), true),
        }
    }

    fn reset_layout(&mut self) {
        if self.base_grid == self.auto_grid {
            self.set_status("already the automatic layout", false);
            return;
        }
        let before = self.base_grid.clone();
        self.base_grid = self.auto_grid.clone();
        self.focus = None;
        self.commit_layout(before, "layout reset");
        self.reveal(false);
    }

    fn undo(&mut self) {
        let Some(prev) = self.undo.pop() else {
            self.set_status("nothing to undo", true);
            return;
        };
        self.redo.push(std::mem::replace(&mut self.base_grid, prev));
        self.rebuild();
        let left = self.undo.len();
        self.persist_layout(&format!("undone ({left} more)"));
    }

    fn redo(&mut self) {
        let Some(next) = self.redo.pop() else {
            self.set_status("nothing to redo", true);
            return;
        };
        self.undo.push(std::mem::replace(&mut self.base_grid, next));
        self.rebuild();
        let left = self.redo.len();
        self.persist_layout(&format!("redone ({left} more)"));
    }

    // ---- mouse dragging

    fn start_drag(&mut self, t: usize) {
        let Some(target) = self.base_grid.pos[t] else { return };
        self.drag = Some(Drag { table: t, before: self.base_grid.clone(), map: self.canvas.map.clone(), target });
        self.set_pointer(Pointer::Grabbing);
        self.rebuild(); // draws the table as "moving"
        self.drag_status();
    }

    /// Follow the mouse: show the diagram as it would look after dropping.
    fn update_drag(&mut self, mx: u16, my: u16) {
        let v = self.view;
        if v.width == 0 || v.height == 0 {
            return;
        }
        // scroll when dragging against the edge of the view
        if mx <= v.x + 1 {
            self.sx = self.sx.saturating_sub(3);
        } else if mx + 2 >= v.right() {
            self.sx += 3;
        }
        if my <= v.y {
            self.sy = self.sy.saturating_sub(2);
        } else if my + 1 >= v.bottom() {
            self.sy += 2;
        }
        let mx = mx.clamp(v.x, v.right() - 1);
        let my = my.clamp(v.y, v.bottom() - 1);
        let (x, y) = (self.sx + (mx - v.x) as usize, self.sy + (my - v.y) as usize);
        let Some(d) = &mut self.drag else { return };
        let (c, r) = d.map.cell_at(x, y);
        let cell = (c.min(d.before.ncols), r.min(d.before.nrows));
        if cell != d.target {
            d.target = cell;
            self.base_grid = moved_grid(&d.before, d.table, cell.0, cell.1);
            self.rebuild();
        }
        self.drag_status();
    }

    fn drag_status(&mut self) {
        let Some(d) = &self.drag else { return };
        let name = &self.schema.tables[d.table].name;
        let swap = match d.before.occupant(d.target.0, d.target.1) {
            Some(u) if u != d.table => format!(", swapping with {}", self.schema.tables[u].name),
            _ => String::new(),
        };
        let msg = if self.base_grid == d.before {
            format!("moving {name}: drag to another spot · esc cancel")
        } else {
            format!("moving {name}{swap} · release to drop · esc cancel")
        };
        self.set_status(msg, false);
    }

    fn finish_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        self.set_pointer(Pointer::Default);
        if self.base_grid == d.before {
            self.status = None; // dropped where it started
            self.rebuild();
            return;
        }
        let name = self.schema.tables[d.table].name.clone();
        self.commit_layout(d.before, &format!("moved {name}"));
    }

    fn cancel_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        self.press = None;
        self.base_grid = d.before;
        self.set_pointer(Pointer::Default);
        self.rebuild();
        self.set_status("move cancelled", false);
    }

    // ---- hover feedback, pointer shape, toolbar

    fn set_pointer(&mut self, p: Pointer) {
        if self.pointer != p {
            self.pointer = p;
            self.pending_pointer = Some(p);
        }
    }

    fn hover(&mut self, mx: u16, my: u16) {
        self.hover_button = self.button_at(mx, my);
        let normal = matches!(self.mode, Mode::Normal);
        self.hover_title = None;
        if normal && self.focus.is_none() && self.drag.is_none() {
            if let Some((x, y)) = self.canvas_point(mx, my) {
                if let Some(Selection::Table(t)) = self.canvas.hit(x, y) {
                    self.hover_title = Some(t);
                }
            }
        }
        let p = if self.hover_button.is_some() {
            Pointer::Hand
        } else if self.hover_title.is_some() {
            Pointer::Grab
        } else {
            Pointer::Default
        };
        self.set_pointer(p);
    }

    fn button_at(&self, mx: u16, my: u16) -> Option<Button> {
        self.buttons.iter().find(|(r, _)| r.contains(Position::new(mx, my))).map(|(_, b)| *b)
    }

    fn press_button(&mut self, b: Button) {
        match b {
            Button::NewTable => self.start_design(),
            Button::Query => self.open_console(None),
            Button::Lint => self.open_lint(),
            Button::Undo => self.undo(),
            Button::Redo => self.redo(),
            Button::Reset => self.reset_layout(),
            Button::Help => self.show_help(),
        }
    }

    // ============================================================== editing

    fn target_names(&self, sel: Selection) -> (String, Option<String>) {
        match sel {
            Selection::Table(t) => (self.schema.tables[t].name.clone(), None),
            Selection::Column(c) => (self.schema.tables[c.table].name.clone(), Some(self.schema.col(c).name.clone())),
        }
    }

    fn description_of(&self, sel: Selection) -> Option<&String> {
        match sel {
            Selection::Table(t) => self.schema.tables[t].description.as_ref(),
            Selection::Column(c) => self.schema.col(c).description.as_ref(),
        }
    }

    fn start_edit(&mut self) {
        let Some(sel) = self.sel else {
            self.set_status("select a column or table title first", true);
            return;
        };
        if self.cfg.read_only {
            self.set_status("read-only mode (started with --read-only)", true);
            return;
        }
        let text: Vec<char> = self.description_of(sel).map(|d| d.chars().collect()).unwrap_or_default();
        let cursor = text.len();
        self.mode = Mode::Edit(Editor { target: sel, text, cursor });
    }

    fn on_edit_key(&mut self, k: KeyEvent) {
        let w = self.edit_w.max(1);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Mode::Edit(mut ed) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
        let len = ed.text.len();
        match k.code {
            KeyCode::Esc => {
                self.set_status("changes discarded", false);
                return;
            }
            KeyCode::Enter => {
                if !self.save_edit(&ed) {
                    self.mode = Mode::Edit(ed); // keep the text on failure
                }
                return;
            }
            KeyCode::Tab => {
                if self.save_edit(&ed) {
                    self.next_missing(1);
                } else {
                    self.mode = Mode::Edit(ed);
                }
                return;
            }
            KeyCode::Char('u') if ctrl => {
                ed.text.clear();
                ed.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                ed.text.insert(ed.cursor, c);
                ed.cursor += 1;
            }
            KeyCode::Backspace if ed.cursor > 0 => {
                ed.cursor -= 1;
                ed.text.remove(ed.cursor);
            }
            KeyCode::Delete if ed.cursor < len => {
                ed.text.remove(ed.cursor);
            }
            KeyCode::Left => ed.cursor = ed.cursor.saturating_sub(1),
            KeyCode::Right => ed.cursor = (ed.cursor + 1).min(len),
            KeyCode::Up => ed.cursor = ed.cursor.saturating_sub(w),
            KeyCode::Down => ed.cursor = (ed.cursor + w).min(len),
            KeyCode::Home => ed.cursor = 0,
            KeyCode::End => ed.cursor = len,
            _ => {}
        }
        self.mode = Mode::Edit(ed);
    }

    fn save_edit(&mut self, ed: &Editor) -> bool {
        let text: String = ed.text.iter().collect::<String>().trim().to_string();
        let (table, column) = self.target_names(ed.target);
        match schema::save_description(&self.cfg.db_path, &self.cfg.desc_table, &table, column.as_deref(), &text) {
            Ok(()) => {
                let value = (!text.is_empty()).then_some(text);
                let t = table_of(ed.target);
                match ed.target {
                    Selection::Table(t) => self.schema.tables[t].description = value,
                    Selection::Column(c) => self.schema.tables[c.table].columns[c.column].description = value,
                }
                self.measures[t] = diagram::measure(&self.schema.tables[t], self.cfg.desc_width);
                self.rebuild();
                self.touch_stamp();
                self.set_status(format!("saved to {}", self.cfg.desc_table), false);
                true
            }
            Err(e) => {
                self.set_status(format!("save failed: {e:#}"), true);
                false
            }
        }
    }

    /// n / N: next table or column without a description (then edit it).
    fn next_missing(&mut self, dir: i64) {
        let mut tables: Vec<usize> = (0..self.schema.tables.len()).collect();
        tables.sort_by_key(|&t| self.base_grid.pos[t]);
        let items: Vec<Selection> = tables
            .iter()
            .flat_map(|&t| {
                std::iter::once(Selection::Table(t)).chain(
                    (0..self.schema.tables[t].columns.len()).map(move |c| Selection::Column(ColRef { table: t, column: c })),
                )
            })
            .collect();
        let len = items.len() as i64;
        let start = self.sel.and_then(|s| items.iter().position(|&x| x == s));
        for k in 1..=len {
            let i = match start {
                Some(s) => (s as i64 + dir * k).rem_euclid(len),
                None if dir > 0 => k - 1,
                None => len - k,
            } as usize;
            if self.description_of(items[i]).is_none() {
                let missing = items.iter().filter(|&&s| self.description_of(s).is_none()).count();
                self.jump(items[i]);
                self.start_edit();
                if matches!(self.mode, Mode::Edit(_)) {
                    self.set_status(format!("{missing} undocumented · tab save & next · enter save · esc skip"), false);
                }
                return;
            }
        }
        self.set_status("every table and column has a description", false);
    }

    // =============================================================== search

    fn start_search(&mut self) {
        self.mode = Mode::Search(Search {
            query: String::new(),
            prev_sel: self.sel,
            prev_scroll: (self.sx, self.sy),
            matches: Vec::new(),
            idx: 0,
        });
    }

    fn search_matches(&self, q: &str) -> Vec<Selection> {
        let q = q.to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let mut tables: Vec<usize> = (0..self.schema.tables.len()).collect();
        tables.sort_by_key(|&t| self.base_grid.pos[t]);
        let rank = |name: &str, base: u8| {
            let n = name.to_lowercase();
            if n == q {
                Some(base)
            } else if n.starts_with(&q) {
                Some(base + 1)
            } else if n.contains(&q) {
                Some(base + 4)
            } else {
                None
            }
        };
        let mut out: Vec<(u8, Selection)> = Vec::new();
        for &t in &tables {
            let table = &self.schema.tables[t];
            if let Some(r) = rank(&table.name, 0) {
                out.push((r, Selection::Table(t)));
            }
            for (ci, c) in table.columns.iter().enumerate() {
                if let Some(r) = rank(&c.name, 2) {
                    out.push((r, Selection::Column(ColRef { table: t, column: ci })));
                }
            }
        }
        out.sort_by_key(|x| x.0);
        out.into_iter().map(|x| x.1).collect()
    }

    fn update_search(&mut self) {
        let Mode::Search(mut s) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
        s.matches = self.search_matches(&s.query);
        s.idx = 0;
        self.apply_search(&s);
        self.mode = Mode::Search(s);
    }

    fn apply_search(&mut self, s: &Search) {
        if let Some(&m) = s.matches.get(s.idx) {
            self.select(Some(m));
            self.reveal(true);
        }
    }

    fn on_search_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Mode::Search(mut s) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
        let n = s.matches.len();
        match k.code {
            KeyCode::Esc => {
                self.select(s.prev_sel);
                (self.sx, self.sy) = s.prev_scroll;
                return;
            }
            KeyCode::Enter => {
                if let (Some(prev), Some(cur)) = (s.prev_sel, self.sel) {
                    if prev != cur {
                        self.history.push(prev);
                    }
                }
                if n == 0 && !s.query.is_empty() {
                    self.set_status(format!("no match for '{}'", s.query), true);
                }
                return;
            }
            KeyCode::Tab | KeyCode::Down if n > 0 => {
                s.idx = (s.idx + 1) % n;
                self.apply_search(&s);
            }
            KeyCode::BackTab | KeyCode::Up if n > 0 => {
                s.idx = (s.idx + n - 1) % n;
                self.apply_search(&s);
            }
            KeyCode::Backspace => {
                s.query.pop();
                self.mode = Mode::Search(s);
                self.update_search();
                return;
            }
            KeyCode::Char('u') if ctrl => {
                s.query.clear();
                self.mode = Mode::Search(s);
                self.update_search();
                return;
            }
            KeyCode::Char(c) if !ctrl => {
                s.query.push(c);
                self.mode = Mode::Search(s);
                self.update_search();
                return;
            }
            _ => {}
        }
        self.mode = Mode::Search(s);
    }

    // =============================================================== popups

    fn on_popup_key(&mut self, k: KeyEvent) {
        let page = (self.view.height as usize).max(1);
        let Mode::Popup(p) = &mut self.mode else { return };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') | KeyCode::Char('i') | KeyCode::Char('?') | KeyCode::Enter => {
                self.mode = Mode::Normal
            }
            KeyCode::Up | KeyCode::Char('w') | KeyCode::Char('k') => p.sy = p.sy.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('s') | KeyCode::Char('j') => p.sy += 1,
            KeyCode::Left | KeyCode::Char('a') | KeyCode::Char('h') => p.sx = p.sx.saturating_sub(4),
            KeyCode::Right | KeyCode::Char('d') | KeyCode::Char('l') => p.sx += 4,
            KeyCode::PageUp => p.sy = p.sy.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => p.sy += page,
            KeyCode::Home | KeyCode::Char('g') => (p.sx, p.sy) = (0, 0),
            _ => {}
        }
    }

    fn popup(&mut self, title: String, lines: Vec<Line<'static>>) {
        self.mode = Mode::Popup(Popup { title, lines, sx: 0, sy: 0 });
    }

    fn selected_table(&mut self) -> Option<usize> {
        let t = self.sel.map(table_of);
        if t.is_none() {
            self.set_status("select a table or column first", true);
        }
        t
    }

    fn show_preview(&mut self) {
        let Some(t) = self.selected_table() else { return };
        let name = self.schema.tables[t].name.clone();
        let p = match schema::preview(&self.cfg.db_path, &name, PREVIEW_ROWS) {
            Ok(p) => p,
            Err(e) => {
                self.set_status(format!("preview failed: {e:#}"), true);
                return;
            }
        };
        let cols = &self.schema.tables[t].columns;
        let color = |h: &str| match cols.iter().find(|c| c.name == h) {
            Some(c) if c.is_pk() => PK_COLOR,
            Some(c) if c.fk => FK_COLOR,
            _ => Color::White,
        };
        let mut lines = console::grid_lines(&p.headers, &p.rows, color);
        if p.rows.is_empty() {
            lines.push(Line::styled("(table is empty)", Style::default().fg(Color::DarkGray)));
        }
        let shown = p.rows.len();
        let title = format!(" {name} · {} rows{} ", p.total, if (p.total as usize) > shown { format!(", first {shown}") } else { String::new() });
        self.popup(title, lines);
    }

    fn show_info(&mut self) {
        let Some(t) = self.selected_table() else { return };
        let table = &self.schema.tables[t];
        let head = Style::default().fg(HL_COLOR).add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines = vec![Line::styled("CREATE statement", head)];
        match &table.sql {
            Some(sql) => lines.extend(sql.lines().map(|l| Line::raw(l.to_string()))),
            None => lines.push(Line::styled("(not available)", dim)),
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Indexes", head));
        if table.indexes.is_empty() {
            lines.push(Line::styled("(none)", dim));
        }
        for i in &table.indexes {
            let kind = match (i.origin.as_str(), i.unique) {
                ("pk", _) => "primary key",
                ("u", _) => "UNIQUE constraint",
                (_, true) => "unique index",
                _ => "index",
            };
            lines.push(Line::from(vec![
                Span::styled(i.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {kind}  "), dim),
                Span::raw(format!("({})", i.columns.join(", "))),
            ]));
            if let Some(sql) = &i.sql {
                lines.extend(sql.lines().map(|l| Line::styled(format!("    {l}"), Style::default().fg(Color::Gray))));
            }
        }
        self.popup(format!(" {} · schema ", table.name), lines);
    }

    fn show_help(&mut self) {
        let key = Style::default().fg(HL_COLOR).add_modifier(Modifier::BOLD);
        let head = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let rows: &[(&str, &[(&str, &str)])] = &[
            ("Select", &[
                ("arrows", "move selection: ↑↓ through rows, ←→ to the next table"),
                ("click", "select column (title = table); click again to close"),
                ("tab", "next column (shift+tab: previous)"),
                ("/", "search tables and columns (tab next match, enter keep, esc cancel)"),
                ("f", "follow relation: FK → PK, PK → referencing columns (repeat to cycle)"),
                ("b", "back to previous selection"),
                ("esc", "clear selection"),
            ]),
            ("Document", &[
                ("e", "on a column: edit its description (enter save, esc discard, ctrl+u clear)"),
                ("n", "next undocumented item, opens the editor (shift+n: previous)"),
                ("tab", "while editing a description: save and go to the next undocumented item"),
            ]),
            ("Inspect", &[
                (":", "query console (read-only): enter runs, ↑↓ history, esc closes"),
                ("!", "lint: broken references, missing indexes, tables without a key…"),
                ("p", "preview the first rows of the table"),
                ("i", "CREATE statement and indexes"),
                ("x", "export Mermaid / Graphviz / Markdown / SQL script next to the database"),
            ]),
            ("Tables", &[
                ("m", "table menu (also right-click): edit, colour, duplicate, delete, preview, query…"),
                ("t", "new table: ↓ past the last row adds a column, enter creates it"),
                ("e", "on a table: edit it (columns, constraints, indexes)"),
                ("del", "delete the selected table (asks first)"),
            ]),
            ("Arrange", &[
                ("z", "focus mode: selected table and its neighbours only"),
                ("space", "collapse / expand selected table (keys only)"),
                ("- / +", "collapse / expand all tables"),
                ("shift+arrows", "move the selected table on the grid (also shift+w a s d)"),
                ("drag title", "move a table with the mouse (live preview, esc cancels)"),
                ("u", "undo a layout change (shift+u: redo; also ctrl+z / ctrl+y and the buttons)"),
                ("shift+r", "reset to automatic layout (undoable)"),
            ]),
            ("View", &[
                ("wasd, h j k l", "pan (shift+h / shift+l: half a page; pgup/pgdn)"),
                ("drag, wheel", "pan (shift+wheel horizontal)"),
                ("c / enter", "center on selection"),
                ("g", "top-left (shift+g: bottom-right)"),
                ("q", "quit"),
            ]),
        ];
        let mut lines = Vec::new();
        for (section, keys) in rows {
            lines.push(Line::styled(section.to_string(), head));
            for (k, what) in keys.iter() {
                lines.push(Line::from(vec![Span::styled(format!("  {k:<16}"), key), Span::raw(what.to_string())]));
            }
            lines.push(Line::raw(""));
        }
        self.popup(" keys ".into(), lines);
    }

    // ============================================================= designer

    /// e: a table opens the table editor, a column its description.
    fn edit_selected(&mut self) {
        match self.sel {
            Some(Selection::Table(t)) => {
                if self.cfg.read_only {
                    self.set_status("read-only mode (started with --read-only)", true);
                    return;
                }
                let color = self.table_color(t);
                self.mode = Mode::Design(Box::new(Draft::for_table(&self.schema, t, &self.cfg.desc_table, color.as_deref())));
            }
            _ => self.start_edit(),
        }
    }

    fn start_design(&mut self) {
        if self.cfg.read_only {
            self.set_status("read-only mode (started with --read-only)", true);
            return;
        }
        self.mode = Mode::Design(Box::new(Draft::new(&self.cfg.desc_table)));
    }

    fn on_design_key(&mut self, k: KeyEvent) {
        let Mode::Design(mut d) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
        match d.on_key(k, &self.schema) {
            Outcome::Continue => self.mode = Mode::Design(d),
            Outcome::Cancel => self.set_status("new table discarded", false),
            Outcome::Submit => self.create_table(d),
        }
    }

    fn apply_edit(&mut self, mut d: Box<Draft>) {
        let Some(ed) = &d.editing else { return };
        let old = ed.orig_name.clone();
        let new = d.table_name();
        let change = match d.change(&self.schema) {
            Ok(c) => c,
            Err(e) => {
                d.error = Some(e);
                self.mode = Mode::Design(d);
                return;
            }
        };
        let descs = d.column_descriptions();
        let empty: Vec<(String, String)> = Vec::new();
        let result = match &change {
            Change::Nothing => {
                self.set_status("no changes", false);
                return;
            }
            Change::DescriptionsOnly => schema::apply_table_change(
                &self.cfg.db_path,
                &schema::TableChange {
                    old_name: &old,
                    new_name: &old,
                    renames: &empty,
                    dropped: &[],
                    rebuild: None,
                    indexes: None,
                    desc_table: &self.cfg.desc_table,
                    table_desc: &d.desc,
                    column_descs: &descs,
                    color: Some(d.color),
                },
            ),
            Change::Apply(plan) => schema::apply_table_change(
                &self.cfg.db_path,
                &schema::TableChange {
                    old_name: &old,
                    new_name: &new,
                    renames: &plan.renames,
                    dropped: &plan.dropped,
                    rebuild: plan.rebuild.as_ref().map(|(sql, keep)| (sql.as_str(), keep.as_slice())),
                    indexes: plan.indexes.as_deref(),
                    desc_table: &self.cfg.desc_table,
                    table_desc: &d.desc,
                    column_descs: &descs,
                    color: Some(d.color),
                },
            ),
        };
        if let Err(e) = result {
            d.error = Some(format!("nothing was changed: {e:#}"));
            self.mode = Mode::Design(d);
            return;
        }
        let renamed = (old != new).then(|| (old.clone(), new.clone()));
        let what = match change {
            Change::DescriptionsOnly => format!("saved {new} (descriptions / colour)"),
            _ => format!("updated table {new}"),
        };
        match self.reload_schema(Some((new.clone(), None)), renamed) {
            Ok(()) => self.set_status(what, false),
            Err(e) => self.set_status(format!("{what}, but reloading failed: {e:#}"), true),
        }
    }

    fn create_table(&mut self, mut d: Box<Draft>) {
        if d.editing.is_some() {
            self.apply_edit(d);
            return;
        }
        let built = d.sql(&self.schema).and_then(|sql| Ok((sql, d.index_sqls(&self.schema)?)));
        let (sql, index_sqls) = match built {
            Ok(x) => x,
            Err(e) => {
                d.error = Some(e);
                self.mode = Mode::Design(d);
                return;
            }
        };
        let name = d.table_name();
        let created = schema::create_table(
            &self.cfg.db_path,
            &schema::NewTable {
                name: &name,
                sql: &sql,
                index_sqls: &index_sqls,
                copy_rows_from: None,
                desc_table: &self.cfg.desc_table,
                table_desc: &d.desc,
                column_descs: &d.column_descriptions(),
                color: Some(d.color),
            },
        );
        if let Err(e) = created {
            d.error = Some(format!("SQLite refused it: {e:#}"));
            self.mode = Mode::Design(d);
            return;
        }
        match self.reload_schema(Some((name.clone(), None)), None) {
            Ok(()) => self.set_status(format!("created table {name} · x then s exports the full SQL"), false),
            Err(e) => self.set_status(format!("created {name}, but reloading failed: {e:#}"), true),
        }
    }

    // ========================================================= table menu

    fn open_menu(&mut self) {
        let Some(t) = self.sel.map(table_of) else {
            self.set_status("select a table first (click it, or the arrow keys)", true);
            return;
        };
        let ro = self.cfg.read_only;
        let mut items = Vec::new();
        let mut add = |key, label: &str, action, enabled: bool| {
            if enabled {
                items.push(MenuItem { key, label: label.to_string(), action, swatch: None });
            }
        };
        add('e', "edit table", MenuAction::Edit, !ro);
        add('c', "colour…", MenuAction::Colors, true);
        add('d', "duplicate (structure only)", MenuAction::Duplicate { with_rows: false }, !ro);
        add('r', "duplicate with rows", MenuAction::Duplicate { with_rows: true }, !ro);
        add('x', "delete table…", MenuAction::Delete, !ro);
        add('p', "preview rows", MenuAction::Preview, true);
        add('i', "schema and indexes", MenuAction::Info, true);
        add('q', "query in the console", MenuAction::Query, true);
        add('z', "focus on this table", MenuAction::Focus, true);
        add(' ', "collapse / expand", MenuAction::Collapse, true);
        let title = format!(" {} ", self.schema.tables[t].name);
        self.mode = Mode::Menu(Menu { table: t, title, items, idx: 0 });
    }

    fn open_color_menu(&mut self, t: usize) {
        let current = self.table_color(t).unwrap_or_else(|| diagram::TABLE_COLORS[0].0.to_string());
        let items: Vec<MenuItem> = diagram::TABLE_COLORS
            .iter()
            .map(|&(name, color)| MenuItem { key: name.chars().next().unwrap(), label: name.into(), action: MenuAction::SetColor(Some(name)), swatch: Some(color) })
            .collect();
        let idx = items.iter().position(|i| matches!(i.action, MenuAction::SetColor(Some(c)) if c == current)).unwrap_or(0);
        self.mode = Mode::Menu(Menu { table: t, title: format!(" colour of {} ", self.schema.tables[t].name), items, idx });
    }

    fn on_menu_key(&mut self, k: KeyEvent) {
        let Mode::Menu(menu) = &mut self.mode else { return };
        let n = menu.items.len();
        let chosen = match k.code {
            KeyCode::Esc | KeyCode::Char('m') if !menu.items.iter().any(|i| i.key == 'm') || k.code == KeyCode::Esc => {
                self.mode = Mode::Normal;
                return;
            }
            KeyCode::Up | KeyCode::Char('w') => {
                menu.idx = (menu.idx + n - 1) % n;
                None
            }
            KeyCode::Down | KeyCode::Char('s') => {
                menu.idx = (menu.idx + 1) % n;
                None
            }
            KeyCode::Enter => menu.items.get(menu.idx).map(|i| i.action),
            KeyCode::Char(c) => menu.items.iter().find(|i| i.key == c).map(|i| i.action),
            _ => None,
        };
        let t = menu.table;
        if let Some(action) = chosen {
            self.mode = Mode::Normal;
            self.menu_action(t, action);
        }
    }

    fn menu_action(&mut self, t: usize, action: MenuAction) {
        let table_sel = Some(Selection::Table(t));
        match action {
            MenuAction::Edit => {
                self.select(table_sel);
                self.edit_selected();
            }
            MenuAction::Colors => self.open_color_menu(t),
            MenuAction::SetColor(c) => self.set_color(t, c),
            MenuAction::Duplicate { with_rows } => {
                let base = format!("{}_copy", self.schema.tables[t].name);
                let mut name = base.clone();
                let mut i = 2;
                while self.schema.find_table(&name).is_some() {
                    name = format!("{base}{i}");
                    i += 1;
                }
                let label = if with_rows { "copy with rows, named" } else { "copy of the structure, named" };
                self.mode = Mode::Prompt(Prompt { label: label.into(), text: name, action: PromptAction::Duplicate { table: t, with_rows } });
            }
            MenuAction::Delete => self.confirm_delete(t),
            MenuAction::Preview => {
                self.select(table_sel);
                self.show_preview();
            }
            MenuAction::Info => {
                self.select(table_sel);
                self.show_info();
            }
            MenuAction::Query => {
                let name = schema::ident_sql(&self.schema.tables[t].name);
                self.open_console(Some(format!("SELECT * FROM {name} LIMIT 100")));
            }
            MenuAction::Focus => {
                self.select(table_sel);
                if self.focus.is_some() {
                    self.focus = None;
                }
                self.toggle_focus();
            }
            MenuAction::Collapse => {
                self.select(table_sel);
                self.toggle_collapse();
            }
        }
    }

    /// The table's stored colour name, if any.
    fn table_color(&self, t: usize) -> Option<String> {
        self.colors.get(&self.schema.tables[t].name.to_lowercase()).cloned()
    }

    fn set_color(&mut self, t: usize, color: Option<&'static str>) {
        // the default colour is the same as none
        let color = color.filter(|c| *c != diagram::TABLE_COLORS[0].0);
        let name = self.schema.tables[t].name.clone();
        match color {
            Some(c) => self.colors.insert(name.to_lowercase(), c.to_string()),
            None => self.colors.remove(&name.to_lowercase()),
        };
        self.rebuild();
        if self.cfg.read_only {
            self.set_status("colour set for this session (read-only: not saved)", false);
            return;
        }
        match schema::save_color(&self.cfg.db_path, &name, color) {
            Ok(()) => self.set_status(format!("{name}: {}", color.unwrap_or(diagram::TABLE_COLORS[0].0)), false),
            Err(e) => self.set_status(format!("saving the colour failed: {e:#}"), true),
        }
        self.touch_stamp();
    }

    fn confirm_delete(&mut self, t: usize) {
        if self.cfg.read_only {
            self.set_status("read-only mode (started with --read-only)", true);
            return;
        }
        let table = &self.schema.tables[t];
        let rows = match table.row_count {
            Some(1) => "1 row".to_string(),
            Some(n) => format!("{n} rows"),
            None => "its rows".into(),
        };
        let mut lines = vec![format!("Delete table {} and {rows}?", table.name)];
        let refs: Vec<String> = self.schema.fks.iter().filter(|f| f.to.table == t && f.from.table != t).map(|f| self.schema.qualified(f.from)).collect();
        if !refs.is_empty() {
            lines.push(format!("{} point{} at it; those references will lead nowhere.", refs.join(", "), if refs.len() == 1 { "s" } else { "" }));
        }
        lines.push("This can't be undone here: make a copy of the database first if unsure.".into());
        lines.push(String::new());
        lines.push("y delete · any other key cancel".into());
        self.mode = Mode::Confirm(Confirm { lines, action: ConfirmAction::DeleteTable(t) });
    }

    fn on_confirm_key(&mut self, k: KeyEvent) {
        let Mode::Confirm(c) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
        if k.code != KeyCode::Char('y') {
            self.set_status("cancelled", false);
            return;
        }
        match c.action {
            ConfirmAction::DeleteTable(t) => {
                let name = self.schema.tables[t].name.clone();
                if let Err(e) = schema::delete_table(&self.cfg.db_path, &name, &self.cfg.desc_table) {
                    self.set_status(format!("deleting {name} failed: {e:#}"), true);
                    return;
                }
                self.sel = None;
                match self.reload_schema(None, None) {
                    Ok(()) => self.set_status(format!("deleted table {name}"), false),
                    Err(e) => self.set_status(format!("deleted {name}, but reloading failed: {e:#}"), true),
                }
            }
        }
    }

    fn on_prompt_key(&mut self, k: KeyEvent) {
        let Mode::Prompt(mut p) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
        match k.code {
            KeyCode::Esc => {
                self.set_status("cancelled", false);
                return;
            }
            KeyCode::Enter => {
                let PromptAction::Duplicate { table, with_rows } = p.action;
                if let Err(e) = self.duplicate(table, p.text.trim(), with_rows) {
                    self.set_status(e, true);
                    self.mode = Mode::Prompt(p);
                }
                return;
            }
            KeyCode::Backspace => {
                p.text.pop();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => p.text.clear(),
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => p.text.push(c),
            _ => {}
        }
        self.status = None;
        self.mode = Mode::Prompt(p);
    }

    fn duplicate(&mut self, t: usize, name: &str, with_rows: bool) -> std::result::Result<(), String> {
        let color = self.table_color(t);
        let d = Draft::duplicate_of(&self.schema, t, name, &self.cfg.desc_table, color.as_deref());
        let sql = d.sql(&self.schema)?;
        let index_sqls = d.index_sqls(&self.schema)?;
        let from = self.schema.tables[t].name.clone();
        let cols = d.column_names();
        schema::create_table(
            &self.cfg.db_path,
            &schema::NewTable {
                name,
                sql: &sql,
                index_sqls: &index_sqls,
                copy_rows_from: with_rows.then_some((from.as_str(), cols.as_slice())),
                desc_table: &self.cfg.desc_table,
                table_desc: &d.desc,
                column_descs: &d.column_descriptions(),
                color: Some(d.color),
            },
        )
        .map_err(|e| format!("copying failed, nothing was created: {e:#}"))?;
        let lost = Draft::for_table(&self.schema, t, &self.cfg.desc_table, None).editing.map(|e| e.lost).unwrap_or_default();
        self.reload_schema(Some((name.to_string(), None)), None).map_err(|e| format!("{e:#}"))?;
        let note = if lost.is_empty() { String::new() } else { format!(" (without {})", lost.join(", ")) };
        self.set_status(format!("created {name} as a copy of {from}{note}"), false);
        Ok(())
    }

    // ================================================== lint and console

    fn open_lint(&mut self) {
        let issues = lint::run(&self.schema, &self.cfg.db_path);
        self.lint_counts = lint::counts(&issues);
        self.mode = Mode::Lint(LintView { issues, idx: 0 });
    }

    fn on_lint_key(&mut self, k: KeyEvent) {
        let Mode::Lint(v) = &mut self.mode else { return };
        let n = v.issues.len().max(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('!') | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('w') => v.idx = (v.idx + n - 1) % n,
            KeyCode::Down | KeyCode::Char('s') => v.idx = (v.idx + 1) % n,
            KeyCode::Char('r') => self.open_lint(),
            KeyCode::Enter => {
                let target = v.issues.get(v.idx).and_then(|i| i.target);
                self.mode = Mode::Normal;
                if let Some(t) = target {
                    self.jump(t);
                }
            }
            _ => {}
        }
    }

    fn open_console(&mut self, query: Option<String>) {
        match query {
            Some(q) => self.console.set_query(&q),
            // a fresh console starts with the selected table
            None if self.console.is_fresh() => {
                if let Some(sel) = self.sel {
                    let name = schema::ident_sql(&self.schema.tables[table_of(sel)].name);
                    self.console.set_query(&format!("SELECT * FROM {name} LIMIT 100"));
                }
            }
            None => {}
        }
        self.mode = Mode::Console;
    }

    // ========================================================= auto-reload

    fn touch_stamp(&mut self) {
        self.file_stamp = schema::file_stamp(&self.cfg.db_path);
    }

    /// Reload when another program changed the database (waits until
    /// you're not in the middle of something).
    fn check_disk(&mut self) {
        self.last_disk_check = std::time::Instant::now();
        let now = schema::file_stamp(&self.cfg.db_path);
        if now != self.file_stamp {
            self.file_stamp = now;
            self.pending_reload = true;
        }
        let idle = matches!(self.mode, Mode::Normal | Mode::Popup(_) | Mode::Lint(_)) && self.drag.is_none();
        if self.pending_reload && idle {
            self.pending_reload = false;
            match self.reload_schema(None, None) {
                Ok(()) => self.set_status("reloaded: the database changed on disk", false),
                Err(e) => self.set_status(format!("the database changed on disk, but reloading failed: {e:#}"), true),
            }
        }
    }

    fn layout_entries(&self) -> Vec<(String, usize, usize)> {
        self.schema
            .tables
            .iter()
            .zip(&self.base_grid.pos)
            .filter_map(|(t, p)| p.map(|(c, r)| (t.name.clone(), c, r)))
            .collect()
    }

    /// Read the schema again after it changed, keeping the user's layout,
    /// undo history and collapsed tables (matched by table name).
    /// `select`: (table, column) to select afterwards; None keeps the current selection.
    fn reload_schema(&mut self, select: Option<(String, Option<String>)>, renamed: Option<(String, String)>) -> Result<()> {
        let new = schema::load(&self.cfg.db_path, &self.cfg.desc_table)?;
        let rename = |n: &str| match &renamed {
            Some((from, to)) if n == from => to.clone(),
            _ => n.to_string(),
        };
        let keep = select.or_else(|| {
            self.sel.map(|s| match s {
                Selection::Table(t) => (rename(&self.schema.tables[t].name), None),
                Selection::Column(c) => (rename(&self.schema.tables[c.table].name), Some(self.schema.col(c).name.clone())),
            })
        });
        let focus_name = self.focus.map(|t| rename(&self.schema.tables[t].name));
        let new_auto = diagram::compute_grid(&new);
        // old names, with a renamed table under its new name
        let old_names: Vec<String> = self
            .schema
            .tables
            .iter()
            .map(|t| match &renamed {
                Some((from, to)) if t.name == *from => to.to_lowercase(),
                _ => t.name.to_lowercase(),
            })
            .collect();
        let remap = |g: &Grid| -> Grid {
            if *g == self.auto_grid {
                return new_auto.clone(); // automatic layout stays automatic
            }
            let saved = old_names
                .iter()
                .zip(&g.pos)
                .filter_map(|(n, p)| p.map(|p| (n.clone(), p)))
                .collect::<HashMap<_, _>>();
            diagram::with_saved(&new, &new_auto, &saved)
        };
        let custom = self.base_grid != self.auto_grid;
        let base = remap(&self.base_grid);
        let undo: Vec<Grid> = self.undo.iter().map(&remap).collect();
        let redo: Vec<Grid> = self.redo.iter().map(remap).collect();
        let collapsed: HashSet<usize> = self.collapsed.iter().filter_map(|&t| new.find_table(&old_names[t])).collect();

        self.measures = new.tables.iter().map(|t| diagram::measure(t, self.cfg.desc_width)).collect();
        self.sel = keep.and_then(|(t, c)| {
            let ti = new.find_table(&t)?;
            match c {
                None => Some(Selection::Table(ti)),
                Some(c) => new.tables[ti]
                    .columns
                    .iter()
                    .position(|x| x.name.eq_ignore_ascii_case(&c))
                    .map(|ci| Selection::Column(ColRef { table: ti, column: ci }))
                    .or(Some(Selection::Table(ti))),
            }
        });
        let focus = focus_name.and_then(|n| new.find_table(&n));
        self.schema = new;
        self.auto_grid = new_auto;
        self.base_grid = base;
        self.undo = undo;
        self.redo = redo;
        self.collapsed = collapsed;
        self.focus = focus;
        self.history.clear();
        self.follow = None;
        if !self.cfg.read_only {
            self.colors = schema::load_colors(&self.cfg.db_path).unwrap_or_default();
        }
        self.lint_counts = lint::counts(&lint::run(&self.schema, &self.cfg.db_path));
        self.rebuild();
        self.reveal(false);
        if custom && !self.cfg.read_only {
            schema::save_layout(&self.cfg.db_path, &self.layout_entries())?;
        }
        self.touch_stamp();
        Ok(())
    }

    // =============================================================== export

    fn on_export_key(&mut self, k: KeyEvent) {
        self.mode = Mode::Normal;
        let formats: Vec<Format> = match k.code {
            KeyCode::Char('m') => vec![Format::Mermaid],
            KeyCode::Char('g') => vec![Format::Graphviz],
            KeyCode::Char('d') => vec![Format::Markdown],
            KeyCode::Char('s') => vec![Format::Sql],
            KeyCode::Char('a') => Format::ALL.to_vec(),
            _ => return,
        };
        let db = Path::new(&self.cfg.db_path);
        let stem = db.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "schema".into());
        let dir = db.parent().unwrap_or(Path::new(""));
        let mut written = Vec::new();
        let layout = if self.base_grid == self.auto_grid { Vec::new() } else { self.layout_entries() };
        let colors: Vec<(String, String)> = self
            .schema
            .tables
            .iter()
            .filter_map(|t| self.colors.get(&t.name.to_lowercase()).map(|c| (t.name.clone(), c.clone())))
            .collect();
        let ctx = ExportCtx {
            title: &self.cfg.db_name,
            desc_width: self.cfg.desc_width,
            desc_table: &self.cfg.desc_table,
            layout: &layout,
            colors: &colors,
        };
        for f in formats {
            let path = dir.join(format!("{stem}.{}", f.extension()));
            let content = f.render(&self.schema, &ctx);
            if let Err(e) = std::fs::write(&path, content) {
                self.set_status(format!("export failed: {}: {e}", path.display()), true);
                return;
            }
            written.push(path.display().to_string());
        }
        self.set_status(format!("wrote {}", written.join(", ")), false);
    }

    // =============================================================== render

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let width = area.width as usize;

        // bottom panel: editor, or details of the selection
        let mut cursor: Option<(u16, u16)> = None;
        let (panel, title, color) = if let Mode::Edit(ed) = &self.mode {
            let w = width.saturating_sub(1).max(1);
            self.edit_w = w;
            // hard-wrapped by characters so the cursor maps 1:1 to a cell
            let nlines = ed.text.len() / w + 1;
            let body = nlines.min((area.height as usize / 2).max(1));
            let (crow, ccol) = (ed.cursor / w, ed.cursor % w);
            let first = crow.saturating_sub(body - 1);
            let (table, column) = self.target_names(ed.target);
            let what = match column {
                Some(c) => format!("{table}.{c}"),
                None => format!("{table} (table)"),
            };
            let mut lines = vec![Line::from(vec![
                Span::styled("editing ", Style::default().fg(Color::DarkGray)),
                Span::styled(what, Style::default().add_modifier(Modifier::BOLD)),
            ])];
            for i in first..first + body {
                let a = (i * w).min(ed.text.len());
                let b = ((i + 1) * w).min(ed.text.len());
                lines.push(Line::raw(ed.text[a..b].iter().collect::<String>()));
            }
            cursor = Some((ccol as u16, (2 + crow - first) as u16));
            (lines, " edit description ", Color::Yellow)
        } else {
            let mut lines = self.detail_lines(width);
            let max = (area.height as usize / 3).max(2);
            lines.truncate(max);
            (lines, " details ", HL_COLOR)
        };
        let panel_h = if panel.is_empty() { 0 } else { panel.len() as u16 + 1 };

        let [header, diag, panel_area, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(panel_h),
            Constraint::Length(1),
        ])
        .areas(area);
        self.view = diag;
        self.clamp();
        self.draw_header(f, header);
        // drag handle feedback: the hovered title lights up
        let handle = self.hover_title.and_then(|t| {
            let (x, y, w, _) = self.canvas.table_rect[t]?;
            Some((x + 1, y, (self.schema.tables[t].name.chars().count() + 2).min(w.saturating_sub(2))))
        });
        f.render_widget(DiagramView { canvas: &self.canvas, sx: self.sx, sy: self.sy, handle }, diag);
        if panel_h > 0 {
            let block = Block::new().borders(Borders::TOP).border_style(Style::default().fg(color)).title(title);
            f.render_widget(Paragraph::new(panel).block(block), panel_area);
        }
        if let Some((x, y)) = cursor {
            f.set_cursor_position(Position::new(panel_area.x + x, panel_area.y + y));
        }

        let (line, footer_cursor) = self.footer();
        f.render_widget(line, footer);
        if let Some(x) = footer_cursor {
            f.set_cursor_position(Position::new(footer.x + x.min(footer.width.saturating_sub(1)), footer.y));
        }

        if let Mode::Popup(p) = &mut self.mode {
            let w = (area.width * 9 / 10).max(10).min(area.width);
            let h = (area.height * 8 / 10).max(3).min(area.height);
            let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
            let inner_h = h.saturating_sub(2) as usize;
            let widest = p.lines.iter().map(|l| l.width()).max().unwrap_or(0);
            p.sy = p.sy.min(p.lines.len().saturating_sub(inner_h));
            p.sx = p.sx.min(widest.saturating_sub(w.saturating_sub(2) as usize));
            let block = Block::bordered()
                .border_style(Style::default().fg(HL_COLOR))
                .title(p.title.clone())
                .title_bottom(Line::styled(" wasd/arrows scroll · esc close ", Style::default().fg(Color::DarkGray)));
            f.render_widget(Clear, rect);
            f.render_widget(
                Paragraph::new(p.lines.clone()).block(block).scroll((p.sy as u16, p.sx as u16)),
                rect,
            );
        }
        if let Mode::Design(d) = &mut self.mode {
            d.draw(f, area, &self.schema);
        }
        let below_header = Rect::new(area.x, area.y + area.height.min(1), area.width, area.height.saturating_sub(1));
        if matches!(self.mode, Mode::Console) {
            self.console.draw(f, below_header);
        }
        self.draw_overlays(f, area);
    }

    /// Menus, confirmations and the lint list, on top of everything.
    fn draw_overlays(&mut self, f: &mut Frame, area: Rect) {
        let dim = Style::default().fg(Color::DarkGray);
        let centered = |w: u16, h: u16| {
            let (w, h) = (w.min(area.width), h.min(area.height));
            Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
        };
        match &self.mode {
            Mode::Menu(m) => {
                let w = m.items.iter().map(|i| i.label.chars().count()).max().unwrap_or(10) as u16 + 12;
                let r = centered(w.max(m.title.chars().count() as u16 + 4), m.items.len() as u16 + 2);
                let block = Block::bordered().border_style(Style::default().fg(HL_COLOR)).title(m.title.clone())
                    .title_bottom(Line::styled(" ↑↓/ws enter · esc ", dim));
                let lines: Vec<Line> = m
                    .items
                    .iter()
                    .enumerate()
                    .map(|(i, it)| {
                        let key = if it.key == ' ' { "space".to_string() } else { it.key.to_string() };
                        let sel = i == m.idx;
                        let base = if sel { Style::default().fg(Color::Black).bg(HL_COLOR) } else { Style::default() };
                        let mut spans = vec![Span::styled(format!(" {key:<6}"), base.add_modifier(Modifier::BOLD))];
                        if let Some(c) = it.swatch {
                            spans.push(Span::styled("██ ", Style::default().fg(c)));
                        }
                        spans.push(Span::styled(format!("{:<w$}", it.label, w = w as usize), base));
                        Line::from(spans)
                    })
                    .collect();
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(block), r);
            }
            Mode::Confirm(c) => {
                let w = c.lines.iter().map(|l| l.chars().count()).max().unwrap_or(20) as u16 + 4;
                let r = centered(w, c.lines.len() as u16 + 2);
                let block = Block::bordered().border_style(Style::default().fg(Color::Red)).title(" are you sure? ");
                let lines: Vec<Line> = c.lines.iter().enumerate().map(|(i, l)| {
                    let st = if i == 0 { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
                    Line::styled(format!(" {l}"), st)
                }).collect();
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(block), r);
            }
            Mode::Lint(v) => {
                let r = centered(area.width * 9 / 10, area.height * 7 / 10);
                let (e, w) = lint::counts(&v.issues);
                let title = format!(" lint: {e} error(s), {w} warning(s) ");
                let block = Block::bordered().border_style(Style::default().fg(HL_COLOR)).title(title)
                    .title_bottom(Line::styled(" ↑↓/ws select · enter go to it · r run again · esc close ", dim));
                let inner_h = block.inner(r).height as usize;
                let first = v.idx.saturating_sub(inner_h.saturating_sub(1));
                let mut lines: Vec<Line> = v
                    .issues
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(inner_h)
                    .map(|(i, is)| {
                        let (icon, color) = match is.severity {
                            Severity::Error => ("✗", Color::Red),
                            Severity::Warning => ("⚠", Color::Yellow),
                            Severity::Info => ("·", Color::Gray),
                        };
                        let base = if i == v.idx { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
                        Line::from(vec![Span::styled(format!(" {icon} "), base.fg(color)), Span::styled(is.message.clone(), base)])
                    })
                    .collect();
                if v.issues.is_empty() {
                    lines.push(Line::styled(" ✓ nothing to report", Style::default().fg(Color::Green)));
                }
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(lines).block(block), r);
            }
            _ => {}
        }
    }

    /// Top bar: database info on the left, clickable buttons on the right.
    fn draw_header(&mut self, f: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(format!(" {} ", self.cfg.db_name), Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::raw(format!(" {} tables, {} relations ", self.schema.tables.len(), self.schema.fks.len())),
            Span::styled("PK ", Style::default().fg(PK_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled("FK ", Style::default().fg(FK_COLOR).add_modifier(Modifier::BOLD)),
        ];
        if let Some(t) = self.focus {
            spans.push(Span::styled(format!(" focus: {} ", self.schema.tables[t].name), Style::default().fg(Color::Black).bg(HL_COLOR)));
        }
        if !self.schema.warnings.is_empty() {
            spans.push(Span::styled(format!(" ⚠ {} (--dump) ", self.schema.warnings.len()), Style::default().fg(Color::Yellow)));
        }
        f.render_widget(Line::from(spans), area);

        let (errors, warnings) = self.lint_counts;
        let lint_label = match errors + warnings {
            0 => " Lint ✓ ".to_string(),
            n => format!(" Lint {n} "),
        };
        let buttons = [
            (Button::NewTable, " + New table ".to_string(), !self.cfg.read_only),
            (Button::Query, " Query ".to_string(), true),
            (Button::Lint, lint_label, true),
            (Button::Undo, format!(" Undo{} ", count_suffix(self.undo.len())), !self.undo.is_empty()),
            (Button::Redo, format!(" Redo{} ", count_suffix(self.redo.len())), !self.redo.is_empty()),
            (Button::Reset, " Reset layout ".to_string(), self.base_grid != self.auto_grid),
            (Button::Help, " ? Help ".to_string(), true),
        ];
        self.buttons.clear();
        let mut x = area.right();
        for (b, label, enabled) in buttons.into_iter().rev() {
            let w = label.chars().count() as u16;
            if x < area.x + w + 1 {
                break; // no room on a very narrow terminal
            }
            x -= w;
            let rect = Rect::new(x, area.y, w, 1);
            let style = match (enabled, self.hover_button == Some(b)) {
                (false, _) => Style::default().fg(Color::DarkGray).bg(Color::Black),
                (true, false) if b == Button::Lint && errors > 0 => Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD),
                (true, false) if b == Button::Lint && warnings > 0 => Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
                (true, true) => Style::default().fg(Color::Black).bg(HL_COLOR).add_modifier(Modifier::BOLD),
                (true, false) => Style::default().fg(Color::White).bg(Color::DarkGray).add_modifier(Modifier::BOLD),
            };
            f.render_widget(Span::styled(label, style), rect);
            self.buttons.push((rect, b));
            x -= 1;
        }
    }

    /// Footer line, plus the cursor column when it contains an input.
    fn footer(&self) -> (Line<'static>, Option<u16>) {
        let dim = Style::default().fg(Color::DarkGray);
        let key = Style::default().fg(HL_COLOR).add_modifier(Modifier::BOLD);
        match &self.mode {
            Mode::Search(s) => {
                let info = match (s.query.is_empty(), s.matches.len()) {
                    (true, _) => "type to search tables and columns".to_string(),
                    (false, 0) => "no match".to_string(),
                    (false, n) => format!("{}/{n} · tab next · enter keep · esc cancel", s.idx + 1),
                };
                let prefix = format!("/{}", s.query);
                let cursor = prefix.chars().count() as u16;
                return (Line::from(vec![Span::styled(prefix, key), Span::styled(format!("   {info}"), dim)]), Some(cursor));
            }
            Mode::Prompt(p) => {
                let label = format!(" {}: ", p.label);
                let cursor = (label.chars().count() + p.text.chars().count()) as u16;
                let mut spans = vec![Span::styled(label, key), Span::raw(p.text.clone())];
                spans.push(Span::styled("   enter ok · esc cancel", dim));
                if let Some((msg, _)) = &self.status {
                    spans.push(Span::styled(format!("   {msg}"), Style::default().fg(Color::Red)));
                }
                return (Line::from(spans), Some(cursor));
            }
            Mode::Export => {
                let mut spans = vec![Span::styled(" export ", Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))];
                for (k, what) in [("m", "Mermaid"), ("g", "Graphviz"), ("d", "Markdown dictionary"), ("s", "SQL script"), ("a", "all")] {
                    spans.push(Span::styled(format!("  {k}"), key));
                    spans.push(Span::raw(format!(" {what}")));
                }
                spans.push(Span::styled("   esc cancel", dim));
                return (Line::from(spans), None);
            }
            _ => {}
        }

        let mut spans = vec![Span::raw(" ")];
        if let Some((msg, err)) = &self.status {
            let style = match (err, self.drag.is_some()) {
                (_, true) => Style::default().fg(Color::Black).bg(MOVE_COLOR).add_modifier(Modifier::BOLD),
                (true, _) => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                (false, _) => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            };
            spans.push(Span::styled(format!(" {msg} "), style));
            spans.push(Span::raw(" "));
        }
        let hints = if self.drag.is_some() {
            ""
        } else if matches!(self.mode, Mode::Edit(_)) {
            "│ enter save · tab save & next · esc discard · ctrl+u clear"
        } else if self.hover_title.is_some() {
            "│ drag the title to move this table · click to select"
        } else {
            "│ arrows move · m menu · e edit · / search · : query · ! lint · u undo · ? help · q quit"
        };
        spans.push(Span::styled(hints, dim));
        (Line::from(spans), None)
    }

    fn detail_lines(&self, width: usize) -> Vec<Line<'static>> {
        let w = width.saturating_sub(1).max(10);
        let dim = Style::default().fg(Color::DarkGray);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut out = Vec::new();
        let s = &self.schema;

        let (desc, refs, by, extra) = match self.sel {
            None => return out,
            Some(Selection::Column(c)) => {
                let col = s.col(c);
                let mut head = vec![Span::styled(s.qualified(c), bold)];
                if !col.data_type.is_empty() {
                    head.push(Span::styled(format!("  {}", col.data_type), Style::default().fg(Color::Gray)));
                }
                if col.is_pk() {
                    head.push(Span::styled("  PRIMARY KEY", Style::default().fg(PK_COLOR)));
                }
                if col.fk {
                    head.push(Span::styled("  FOREIGN KEY", Style::default().fg(FK_COLOR)));
                }
                if col.unique && !col.is_pk() {
                    head.push(Span::styled("  UNIQUE", Style::default().fg(Color::Gray)));
                }
                if col.not_null {
                    head.push(Span::styled("  NOT NULL", dim));
                }
                if let Some(d) = &col.default {
                    head.push(Span::styled(format!("  DEFAULT {d}"), dim));
                }
                out.push(Line::from(head));
                let refs: Vec<String> = s.fks.iter().filter(|f| f.from == c).map(|f| s.qualified(f.to)).collect();
                let by: Vec<String> = s.fks.iter().filter(|f| f.to == c).map(|f| s.qualified(f.from)).collect();
                (col.description.clone(), refs, by, vec![])
            }
            Some(Selection::Table(t)) => {
                let table = &s.tables[t];
                out.push(Line::from(vec![
                    Span::styled(table.name.clone(), bold),
                    Span::styled(format!("  {} columns", table.columns.len()), dim),
                    Span::styled("   p preview · i schema", dim),
                ]));
                let mut refs: Vec<String> =
                    s.fks.iter().filter(|f| f.from.table == t).map(|f| s.tables[f.to.table].name.clone()).collect();
                let mut by: Vec<String> =
                    s.fks.iter().filter(|f| f.to.table == t).map(|f| s.tables[f.from.table].name.clone()).collect();
                refs.sort();
                refs.dedup();
                by.sort();
                by.dedup();
                let idx: Vec<String> = table
                    .indexes
                    .iter()
                    .map(|i| format!("{}{} ({})", i.name, if i.unique { " UNIQUE" } else { "" }, i.columns.join(", ")))
                    .collect();
                (table.description.clone(), refs, by, idx)
            }
        };

        let mut labelled = |label: &str, items: &[String], color: Color| {
            if items.is_empty() {
                return;
            }
            for (i, l) in wrap(&format!("{label} {}", items.join(", ")), w).into_iter().enumerate() {
                if i == 0 && l.starts_with(label) {
                    let rest = l[label.len()..].to_string();
                    out.push(Line::from(vec![Span::styled(label.to_string(), dim), Span::styled(rest, Style::default().fg(color))]));
                } else {
                    out.push(Line::styled(l, Style::default().fg(color)));
                }
            }
        };
        labelled("references →", &refs, FK_COLOR);
        labelled("referenced by ←", &by, PK_COLOR);
        labelled("indexes:", &extra, Color::Gray);

        match desc {
            Some(d) => out.extend(wrap(&d, w).into_iter().map(Line::raw)),
            None => out.push(Line::styled("(no description · e to add)", dim.add_modifier(Modifier::ITALIC))),
        }
        out
    }
}

fn count_suffix(n: usize) -> String {
    if n == 0 { String::new() } else { format!(" ({n})") }
}

struct DiagramView<'a> {
    canvas: &'a Canvas,
    sx: usize,
    sy: usize,
    /// (x, y, width) on the canvas of a title to show as a drag handle
    handle: Option<(usize, usize, usize)>,
}

impl Widget for DiagramView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for y in 0..area.height {
            for x in 0..area.width {
                let (cx, cy) = (self.sx + x as usize, self.sy + y as usize);
                if let Some(c) = self.canvas.get(cx, cy) {
                    if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                        cell.set_char(c.ch).set_style(c.style);
                        if let Some((hx, hy, hw)) = self.handle {
                            if cy == hy && cx >= hx && cx < hx + hw {
                                cell.set_style(Style::default().fg(Color::Black).bg(MOVE_COLOR).add_modifier(Modifier::BOLD));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::designer::{DraftCol, RefTarget};
    use ratatui::{backend::TestBackend, Terminal};

    /// Each test gets its own copy of the test database, so writes don't leak.
    fn app_copy(name: &str) -> App {
        let path = format!("/tmp/erd_test_{name}.db");
        crate::schema::test_db(&path);
        let schema = crate::schema::load(&path, "_descriptions").unwrap();
        let mut a = App::new(schema, Config {
            db_path: path.clone(),
            db_name: path,
            desc_table: "_descriptions".into(),
            desc_width: 24,
            read_only: false,
            pointer_shapes: true,
        });
        a.view = Rect::new(0, 0, 200, 50);
        a
    }

    fn key(a: &mut App, code: KeyCode) {
        a.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    fn typ(a: &mut App, s: &str) {
        for c in s.chars() {
            key(a, KeyCode::Char(c));
        }
    }
    fn draw(a: &mut App, w: u16, h: u16) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
    }
    fn sel_name(a: &App) -> String {
        match a.sel {
            Some(Selection::Table(t)) => a.schema.tables[t].name.clone(),
            Some(Selection::Column(c)) => a.schema.qualified(c),
            None => "-".into(),
        }
    }

    #[test]
    fn click_selects_and_expands_then_collapses() {
        let mut a = app_copy("click");
        draw(&mut a, 300, 60);
        let target = a.resolve("tasks.status").unwrap();
        let Selection::Column(c) = target else { panic!() };
        let (x, _, _, h_before) = a.canvas.table_rect[c.table].unwrap();
        let y = a.canvas.col_y[&c];
        let (px, py) = screen(&a, x + 3, y);
        a.click(px as u16, py as u16);
        assert_eq!(a.sel, Some(target));
        assert!(a.canvas.table_rect[c.table].unwrap().3 > h_before, "description expanded");
        let (px, py) = screen(&a, x + 3, a.canvas.col_y[&c]);
        a.click(px as u16, py as u16);
        assert_eq!(a.sel, None);
    }

    #[test]
    fn arrow_navigation() {
        let mut a = app_copy("arrows");
        key(&mut a, KeyCode::Down); // nothing selected → first table
        assert!(matches!(a.sel, Some(Selection::Table(_))));
        a.select(a.resolve("users").ok());
        key(&mut a, KeyCode::Down);
        assert_eq!(sel_name(&a), "users.id");
        key(&mut a, KeyCode::Down);
        assert_eq!(sel_name(&a), "users.org_id");
        key(&mut a, KeyCode::Up);
        key(&mut a, KeyCode::Up);
        assert_eq!(sel_name(&a), "users");
        // users.org_id → left lands in organization
        a.select(a.resolve("users.org_id").ok());
        key(&mut a, KeyCode::Left);
        assert!(sel_name(&a).starts_with("organization."), "{}", sel_name(&a));
        key(&mut a, KeyCode::Right);
        assert!(sel_name(&a).starts_with("users."), "{}", sel_name(&a));
        // wasd pans the view (smaller than the diagram) and leaves the selection alone
        let view = a.view;
        a.view = Rect::new(0, 0, 20, 8);
        (a.sx, a.sy) = (0, 0);
        let before = a.sel;
        key(&mut a, KeyCode::Char('d'));
        key(&mut a, KeyCode::Char('s'));
        assert_eq!(a.sel, before);
        assert!(a.sx > 0 && a.sy > 0, "{} {}", a.sx, a.sy);
        a.view = view;
        // walking down off the last column enters the table below
        a.select(a.resolve("organization.created_at").ok());
        key(&mut a, KeyCode::Down);
        assert_eq!(sel_name(&a), "tags");
    }

    #[test]
    fn edit_save_discard_and_persist() {
        let mut a = app_copy("edit");
        a.select(a.resolve("tags.label").ok());
        key(&mut a, KeyCode::Char('E'));
        typ(&mut a, "Tag name");
        draw(&mut a, 120, 40);
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Normal));
        key(&mut a, KeyCode::Char('e'));
        typ(&mut a, "XXX");
        key(&mut a, KeyCode::Esc); // discard
        a.select(a.resolve("tags").ok());
        key(&mut a, KeyCode::Char('e')); // a table: opens the table editor
        match &mut a.mode {
            Mode::Design(d) => d.desc = "Labels".into(),
            _ => panic!("table editor expected"),
        }
        key(&mut a, KeyCode::Enter);
        a.select(a.resolve("users.email").ok());
        key(&mut a, KeyCode::Char('e'));
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        key(&mut a, KeyCode::Enter);

        let r = crate::schema::load(&a.cfg.db_path, "_descriptions").unwrap();
        let t = r.find_table("tags").unwrap();
        assert_eq!(r.tables[t].columns[1].description.as_deref(), Some("Tag name"));
        assert_eq!(r.tables[t].description.as_deref(), Some("Labels"));
        let u = r.find_table("users").unwrap();
        assert_eq!(r.tables[u].columns[2].description, None);
    }

    #[test]
    fn next_missing_walks_and_tab_saves() {
        let mut a = app_copy("missing");
        key(&mut a, KeyCode::Char('n'));
        assert!(matches!(a.mode, Mode::Edit(_)));
        let first = a.sel;
        typ(&mut a, "documented");
        key(&mut a, KeyCode::Tab); // save & next
        assert!(matches!(a.mode, Mode::Edit(_)));
        assert_ne!(a.sel, first);
        key(&mut a, KeyCode::Esc);
        assert_eq!(a.description_of(first.unwrap()).map(|s| s.as_str()), Some("documented"));
    }

    #[test]
    fn search_selects_cycles_and_cancels() {
        let mut a = app_copy("search");
        a.select(a.resolve("tags").ok());
        key(&mut a, KeyCode::Char('/'));
        typ(&mut a, "org");
        assert_eq!(sel_name(&a), "organization"); // table prefix beats column
        key(&mut a, KeyCode::Tab);
        assert!(sel_name(&a).ends_with(".org_id"), "{}", sel_name(&a));
        draw(&mut a, 100, 30);
        key(&mut a, KeyCode::Esc);
        assert_eq!(sel_name(&a), "tags");
        key(&mut a, KeyCode::Char('/'));
        typ(&mut a, "manager");
        key(&mut a, KeyCode::Enter);
        assert_eq!(sel_name(&a), "users.manager_id");
        key(&mut a, KeyCode::Char('b'));
        assert_eq!(sel_name(&a), "tags");
    }

    #[test]
    fn follow_fk_and_cycle_referencers() {
        let mut a = app_copy("follow");
        a.select(a.resolve("comments.author_id").ok());
        key(&mut a, KeyCode::Char('f'));
        assert_eq!(sel_name(&a), "users.id");
        let mut seen = HashSet::new();
        for _ in 0..6 {
            key(&mut a, KeyCode::Char('f'));
            seen.insert(sel_name(&a));
        }
        // users.id is referenced by 4 columns; f cycles through all of them
        assert_eq!(seen.len(), 4, "{seen:?}");
        key(&mut a, KeyCode::Char('b'));
    }

    #[test]
    fn focus_collapse_and_rendering() {
        let mut a = app_copy("focus");
        a.select(a.resolve("tags").ok());
        key(&mut a, KeyCode::Char('z'));
        let shown: Vec<_> = a.order.iter().map(|&t| a.schema.tables[t].name.clone()).collect();
        assert_eq!(shown.len(), 2, "{shown:?}"); // tags + task_tags
        key(&mut a, KeyCode::Right); // walk to task_tags: focus follows
        key(&mut a, KeyCode::Char('z'));
        assert!(a.focus.is_none());
        assert_eq!(a.order.len(), a.schema.tables.len());

        a.select(a.resolve("organization.name").ok());
        key(&mut a, KeyCode::Char(' ')); // collapse; selection moves to table
        let o = a.schema.find_table("organization").unwrap();
        assert!(a.collapsed.contains(&o));
        assert!(a.canvas.to_text().contains("+2 hidden"));
        key(&mut a, KeyCode::Char('-'));
        key(&mut a, KeyCode::Char('+'));
        assert!(a.collapsed.is_empty());
    }

    #[test]
    fn move_table_persists_and_resets() {
        let mut a = app_copy("move");
        let t = a.schema.find_table("tags").unwrap();
        let before = a.base_grid.pos[t].unwrap();
        a.select(Some(Selection::Table(t)));
        key(&mut a, KeyCode::Char('D'));
        a.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)); // shift+arrows move it too
        assert_eq!(a.base_grid.pos[t].unwrap(), before);
        assert_eq!(a.sel, Some(Selection::Table(t)), "shift+arrow doesn't move the selection");
        a.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        let after = a.base_grid.pos[t].unwrap();
        assert_eq!(after, (before.0 + 1, before.1));
        // reload: layout comes back from _erd_layout, and the table is not drawn
        let schema = crate::schema::load(&a.cfg.db_path, "_descriptions").unwrap();
        assert!(schema.find_table("_erd_layout").is_none());
        let b = App::new(schema, Config { db_path: a.cfg.db_path.clone(), db_name: "x".into(), desc_table: "_descriptions".into(), desc_width: 24, read_only: false, pointer_shapes: true });
        assert_eq!(b.base_grid.pos[t], Some(after));
        key(&mut a, KeyCode::Char('R'));
        assert_eq!(a.base_grid.pos[t], Some(before));
        assert!(crate::schema::load_layout(&a.cfg.db_path).unwrap().is_empty());
    }

    #[test]
    fn mouse_drag_moves_table() {
        let mut a = app_copy("drag");
        draw(&mut a, 300, 60);
        let t = a.schema.find_table("tags").unwrap();
        let before = a.base_grid.pos[t].unwrap();
        let (x, y, _, _) = a.canvas.table_rect[t].unwrap();
        let target = a.canvas.table_rect[a.schema.find_table("comments").unwrap()].unwrap();
        let (sx, sy) = screen(&a, x + 3, y);
        let (tx, ty) = screen(&a, target.0 + 2, target.1 + 1);
        mouse(&mut a, MouseEventKind::Down(MouseButton::Left), sx, sy);
        mouse(&mut a, MouseEventKind::Drag(MouseButton::Left), tx, ty);
        mouse(&mut a, MouseEventKind::Up(MouseButton::Left), tx, ty);
        assert_ne!(a.base_grid.pos[t], Some(before));
    }

    #[test]
    fn popups_export_and_tiny_terminals() {
        let mut a = app_copy("popups");
        a.select(a.resolve("tasks").ok());
        for k in ['p', 'i', '?'] {
            key(&mut a, KeyCode::Char(k));
            assert!(matches!(a.mode, Mode::Popup(_)));
            for (w, h) in [(8, 3), (40, 10), (120, 40)] {
                for _ in 0..5 {
                    key(&mut a, KeyCode::Char('s'));
                    key(&mut a, KeyCode::Char('d'));
                }
                draw(&mut a, w, h);
            }
            key(&mut a, KeyCode::Esc);
        }
        key(&mut a, KeyCode::Char('x'));
        key(&mut a, KeyCode::Char('a'));
        for ext in ["mmd", "dot", "md"] {
            let p = format!("/tmp/erd_test_popups.{ext}");
            let s = std::fs::read_to_string(&p).unwrap();
            assert!(s.contains("tasks"), "{p}");
        }
        // cycle everything in small terminals, also while editing long text
        for (w, h) in [(10, 4), (40, 8), (80, 24)] {
            for _ in 0..20 {
                a.cycle(1);
                draw(&mut a, w, h);
            }
            key(&mut a, KeyCode::Char('e'));
            typ(&mut a, &"x".repeat(200));
            key(&mut a, KeyCode::Up);
            draw(&mut a, w, h);
            key(&mut a, KeyCode::Esc);
            key(&mut a, KeyCode::Char('/'));
            typ(&mut a, "id");
            draw(&mut a, w, h);
            key(&mut a, KeyCode::Esc);
        }
    }

    fn mouse(a: &mut App, kind: MouseEventKind, x: usize, y: usize) {
        a.on_mouse(MouseEvent { kind, column: x as u16, row: y as u16, modifiers: KeyModifiers::NONE });
    }

    /// Screen position of a canvas point (after a draw, view is known).
    fn screen(a: &App, x: usize, y: usize) -> (usize, usize) {
        (x - a.sx + a.view.x as usize, y - a.sy + a.view.y as usize)
    }

    #[test]
    fn drag_shows_live_preview_and_esc_cancels() {
        let mut a = app_copy("drag_preview");
        draw(&mut a, 300, 60);
        let t = a.schema.find_table("tags").unwrap();
        let start = a.base_grid.clone();
        let (x, y, _, _) = a.canvas.table_rect[t].unwrap();
        let (sx, sy) = screen(&a, x + 3, y);
        mouse(&mut a, MouseEventKind::Moved, sx, sy);
        assert_eq!(a.hover_title, Some(t));
        assert_eq!(a.pending_pointer, Some(Pointer::Grab));

        let c = a.canvas.table_rect[a.schema.find_table("comments").unwrap()].unwrap();
        let (tx, ty) = screen(&a, c.0 + 2, c.1 + 1);
        mouse(&mut a, MouseEventKind::Down(MouseButton::Left), sx, sy);
        mouse(&mut a, MouseEventKind::Drag(MouseButton::Left), tx, ty);
        // preview: layout already changed, table drawn with a double border
        assert!(a.drag.is_some());
        assert_ne!(a.base_grid, start);
        assert!(a.canvas.to_text().contains('╔'));
        assert_eq!(a.pointer, Pointer::Grabbing);
        assert!(a.status.as_ref().unwrap().0.contains("swapping with comments"));
        draw(&mut a, 300, 60);

        key(&mut a, KeyCode::Esc);
        assert!(a.drag.is_none());
        assert_eq!(a.base_grid, start);
        assert!(a.undo.is_empty());
        assert!(!a.canvas.to_text().contains('╔'));
        mouse(&mut a, MouseEventKind::Up(MouseButton::Left), tx, ty); // stray release: no-op
        assert_eq!(a.base_grid, start);
    }

    #[test]
    fn drop_then_undo_redo_and_reset() {
        let mut a = app_copy("undo");
        draw(&mut a, 300, 60);
        let auto = a.base_grid.clone();
        let t = a.schema.find_table("tags").unwrap();
        let (x, y, _, _) = a.canvas.table_rect[t].unwrap();
        let (sx, sy) = screen(&a, x + 3, y);
        let c = a.canvas.table_rect[a.schema.find_table("comments").unwrap()].unwrap();
        let (tx, ty) = screen(&a, c.0 + 2, c.1 + 1);
        mouse(&mut a, MouseEventKind::Down(MouseButton::Left), sx, sy);
        mouse(&mut a, MouseEventKind::Drag(MouseButton::Left), tx, ty);
        mouse(&mut a, MouseEventKind::Up(MouseButton::Left), tx, ty);
        let moved = a.base_grid.clone();
        assert_ne!(moved, auto);
        assert_eq!(a.undo.len(), 1);
        assert_ne!(a.pointer, Pointer::Grabbing);
        assert!(!crate::schema::load_layout(&a.cfg.db_path).unwrap().is_empty());

        key(&mut a, KeyCode::Char('u'));
        assert_eq!(a.base_grid, auto);
        assert!(crate::schema::load_layout(&a.cfg.db_path).unwrap().is_empty(), "back to automatic: saved layout removed");
        key(&mut a, KeyCode::Char('U'));
        assert_eq!(a.base_grid, moved);

        key(&mut a, KeyCode::Char('R'));
        assert_eq!(a.base_grid, auto);
        key(&mut a, KeyCode::Char('u')); // reset is undoable
        assert_eq!(a.base_grid, moved);

        // the toolbar buttons do the same
        draw(&mut a, 300, 60);
        let (r, _) = *a.buttons.iter().find(|(_, b)| *b == Button::Reset).unwrap();
        mouse(&mut a, MouseEventKind::Moved, r.x as usize, r.y as usize);
        assert_eq!(a.hover_button, Some(Button::Reset));
        mouse(&mut a, MouseEventKind::Down(MouseButton::Left), r.x as usize, r.y as usize);
        mouse(&mut a, MouseEventKind::Up(MouseButton::Left), r.x as usize, r.y as usize);
        assert_eq!(a.base_grid, auto);
        draw(&mut a, 300, 60);
        let (r, _) = *a.buttons.iter().find(|(_, b)| *b == Button::Undo).unwrap();
        mouse(&mut a, MouseEventKind::Down(MouseButton::Left), r.x as usize + 1, r.y as usize);
        mouse(&mut a, MouseEventKind::Up(MouseButton::Left), r.x as usize + 1, r.y as usize);
        assert_eq!(a.base_grid, moved);
        // narrow terminals drop buttons instead of overlapping
        draw(&mut a, 20, 10);
        assert!(a.buttons.iter().all(|(r, _)| r.x < 20));
    }

    #[test]
    fn dropping_in_place_changes_nothing() {
        let mut a = app_copy("inplace");
        draw(&mut a, 300, 60);
        let t = a.schema.find_table("users").unwrap();
        let (x, y, _, _) = a.canvas.table_rect[t].unwrap();
        let (sx, sy) = screen(&a, x + 3, y);
        mouse(&mut a, MouseEventKind::Down(MouseButton::Left), sx, sy);
        mouse(&mut a, MouseEventKind::Drag(MouseButton::Left), sx + 1, sy);
        mouse(&mut a, MouseEventKind::Up(MouseButton::Left), sx + 1, sy);
        assert!(a.undo.is_empty());
        assert!(a.drag.is_none());
        assert!(a.sel.is_none(), "a drag is not a click");
    }

    #[test]
    fn design_a_table_with_keys() {
        let mut a = app_copy("design");
        draw(&mut a, 160, 45);
        key(&mut a, KeyCode::Char('t'));
        assert!(matches!(a.mode, Mode::Design(_)));
        typ(&mut a, "orders");
        key(&mut a, KeyCode::Tab);
        typ(&mut a, "A customer order");
        // row 1 already is: id INTEGER PK. Arrow down past it for a new column.
        key(&mut a, KeyCode::Down); // → colour
        key(&mut a, KeyCode::Down); // → row 1 (id)
        key(&mut a, KeyCode::Down); // → new row 2, Name
        typ(&mut a, "user_id");
        key(&mut a, KeyCode::Tab); // Type
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)); // clear "TEXT"
        key(&mut a, KeyCode::Tab); // PK
        key(&mut a, KeyCode::Tab); // NN: tick it
        key(&mut a, KeyCode::Char(' '));
        for _ in 0..6 { key(&mut a, KeyCode::Tab); } // UQ, AI, Default, Collate, Check → References
        typ(&mut a, "users.i"); // opens picker, filters
        draw(&mut a, 160, 45);
        key(&mut a, KeyCode::Enter); // picks
        for _ in 0..3 { key(&mut a, KeyCode::Tab); } // on delete, on update → Description
        typ(&mut a, "Who placed it");
        // a column with a default
        key(&mut a, KeyCode::Down);
        assert!(matches!(&a.mode, Mode::Design(d) if d.cols.len() == 3));
        typ(&mut a, "status");
        for _ in 0..6 { key(&mut a, KeyCode::Tab); } // type, PK, NN, UQ, AI → Default
        typ(&mut a, "new");
        draw(&mut a, 160, 45);
        draw(&mut a, 30, 10); // tiny terminal doesn't panic
        key(&mut a, KeyCode::Down); // a blank row at the end is ignored
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Normal), "status: {:?}", a.status);

        let t = a.schema.find_table("orders").expect("table created");
        assert_eq!(a.sel, Some(Selection::Table(t)));
        let orders = &a.schema.tables[t];
        assert_eq!(orders.description.as_deref(), Some("A customer order"));
        let uid = &orders.columns[1];
        assert_eq!((uid.name.as_str(), uid.data_type.as_str(), uid.not_null, uid.fk), ("user_id", "INTEGER", true, true));
        assert_eq!(uid.description.as_deref(), Some("Who placed it"));
        assert_eq!(orders.columns[2].default.as_deref(), Some("'new'"));
        let fk = a.schema.fks.iter().find(|f| f.from.table == t).unwrap();
        assert_eq!(a.schema.qualified(fk.to), "users.id");
        assert!(a.canvas.table_rect[t].is_some(), "new table is drawn");
    }

    #[test]
    fn designer_errors_keep_the_form_open() {
        let mut a = app_copy("design_err");
        key(&mut a, KeyCode::Char('t'));
        typ(&mut a, "users"); // exists
        key(&mut a, KeyCode::Enter);
        let Mode::Design(d) = &a.mode else { panic!("form closed") };
        assert!(d.error.as_ref().unwrap().contains("already exists"));
        key(&mut a, KeyCode::Esc); // dirty: asks first
        assert!(matches!(a.mode, Mode::Design(_)));
        key(&mut a, KeyCode::Esc);
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!(crate::schema::load(&a.cfg.db_path, "_descriptions").unwrap().tables.len(), a.schema.tables.len());
    }

    #[test]
    fn creating_a_table_keeps_custom_layout_and_undo() {
        let mut a = app_copy("design_layout");
        let tags = a.schema.find_table("tags").unwrap();
        a.select(Some(Selection::Table(tags)));
        key(&mut a, KeyCode::Char('D')); // custom layout, one undo step
        let tags_pos = a.base_grid.pos[tags];
        key(&mut a, KeyCode::Char('t'));
        typ(&mut a, "aaa_first"); // sorts first: shifts all table indices
        key(&mut a, KeyCode::Enter);
        let tags2 = a.schema.find_table("tags").unwrap();
        assert_ne!(tags, tags2);
        assert_eq!(a.base_grid.pos[tags2], tags_pos, "tags stays where the user put it");
        assert_eq!(a.undo.len(), 1);
        key(&mut a, KeyCode::Char('u'));
        assert_eq!(a.base_grid, a.auto_grid);
    }

    #[test]
    fn sql_export_recreates_the_database() {
        let mut a = app_copy("sqlexport");
        let t = a.schema.find_table("tags").unwrap();
        a.select(Some(Selection::Table(t)));
        key(&mut a, KeyCode::Char('D'));
        key(&mut a, KeyCode::Char('x'));
        key(&mut a, KeyCode::Char('s'));
        let sql = std::fs::read_to_string("/tmp/erd_test_sqlexport.sql").unwrap();
        let fresh = "/tmp/erd_test_sqlexport_fresh.db";
        let _ = std::fs::remove_file(fresh);
        rusqlite::Connection::open(fresh).unwrap().execute_batch(&sql).unwrap();

        let b = crate::schema::load(fresh, "_descriptions").unwrap();
        let names = |s: &Schema| s.tables.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&b), names(&a.schema));
        let fks = |s: &Schema| {
            let mut v: Vec<(String, String)> = s.fks.iter().map(|f| (s.qualified(f.from), s.qualified(f.to))).collect();
            v.sort();
            v
        };
        assert_eq!(fks(&b), fks(&a.schema));
        for (x, y) in a.schema.tables.iter().zip(&b.tables) {
            assert_eq!(x.description, y.description);
            for (c, d) in x.columns.iter().zip(&y.columns) {
                assert_eq!((&c.name, &c.data_type, c.pk_index, c.not_null, &c.default, &c.description),
                           (&d.name, &d.data_type, d.pk_index, d.not_null, &d.default, &d.description));
            }
        }
        assert_eq!(crate::schema::load_layout(fresh).unwrap(), crate::schema::load_layout(&a.cfg.db_path).unwrap());
    }

    fn seed(a: &App) {
        rusqlite::Connection::open(&a.cfg.db_path).unwrap().execute_batch(
            "INSERT INTO organization (id, name) VALUES (1, 'Acme'), (2, 'Globex');
             INSERT INTO users (id, org_id, email) VALUES (10, 1, 'a@x'), (11, 2, 'b@x');
             INSERT INTO projects (id, org_id, owner_id, title) VALUES (100, 1, 10, 'Rocket');
             CREATE INDEX projects_title ON projects(title);",
        ).unwrap();
    }

    fn query(a: &App, sql: &str) -> Vec<String> {
        let c = rusqlite::Connection::open(&a.cfg.db_path).unwrap();
        let mut st = c.prepare(sql).unwrap();
        let n = st.column_count();
        let show = |v: rusqlite::types::ValueRef| match v {
            rusqlite::types::ValueRef::Text(t) => format!("Text({:?})", String::from_utf8_lossy(t)),
            other => format!("{other:?}"),
        };
        let rows = st.query_map([], |r| Ok((0..n).map(|i| show(r.get_ref(i).unwrap())).collect::<Vec<_>>().join("|"))).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn open_editor(a: &mut App, table: &str) {
        a.select(a.resolve(table).ok());
        key(a, KeyCode::Char('e'));
        assert!(matches!(a.mode, Mode::Design(_)), "e on a table opens the editor");
    }

    fn draft(a: &mut App) -> &mut Draft {
        match &mut a.mode {
            Mode::Design(d) => d,
            _ => panic!("editor not open (status: {:?})", a.status),
        }
    }

    #[test]
    fn e_on_a_column_still_edits_just_the_description() {
        let mut a = app_copy("e_column");
        a.select(a.resolve("users.email").ok());
        key(&mut a, KeyCode::Char('e'));
        assert!(matches!(a.mode, Mode::Edit(_)));
    }

    #[test]
    fn edit_table_keeps_data_and_follows_renames() {
        let mut a = app_copy("edit_table");
        seed(&a);
        open_editor(&mut a, "projects");
        {
            let d = draft(&mut a);
            assert_eq!(d.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "org_id", "owner_id", "title"]);
            d.name = "project".into(); // rename the table
            d.cols[3].name = "name".into(); // rename a column (the index uses it)
            d.cols[3].not_null = true;
            d.cols.push(DraftCol { name: "budget".into(), ty: "REAL".into(), default: "0".into(), ..Default::default() });
            d.desc = "Work we do".into();
        }
        draw(&mut a, 160, 45);
        key(&mut a, KeyCode::Enter); // nothing is lost: no confirmation needed
        assert!(matches!(a.mode, Mode::Normal), "{:?}", a.status);

        assert_eq!(query(&a, "SELECT id, org_id, owner_id, name, budget FROM project"), ["Integer(100)|Integer(1)|Integer(10)|Text(\"Rocket\")|Real(0.0)"]);
        // other tables' references followed the rename; the index came back on the renamed column
        let tasks = query(&a, "SELECT sql FROM sqlite_master WHERE name = 'tasks'");
        assert!(tasks[0].contains("project") && !tasks[0].contains("projects"), "{tasks:?}");
        assert_eq!(query(&a, "SELECT name FROM pragma_index_info('projects_title')"), ["Text(\"name\")"]);
        assert!(query(&a, "SELECT * FROM pragma_foreign_key_check").is_empty());
        let t = a.schema.find_table("project").unwrap();
        assert_eq!(a.sel, Some(Selection::Table(t)));
        assert_eq!(a.schema.tables[t].description.as_deref(), Some("Work we do"));
        assert!(a.schema.fks.iter().any(|f| a.schema.qualified(f.from) == "tasks.project_id" && f.to.table == t));
    }

    #[test]
    fn description_only_edit_does_not_rebuild() {
        let mut a = app_copy("edit_desc");
        let before = query(&a, "SELECT sql FROM sqlite_master WHERE name = 'users'");
        open_editor(&mut a, "users");
        draft(&mut a).cols[2].desc = "Primary contact address".into();
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!(query(&a, "SELECT sql FROM sqlite_master WHERE name = 'users'"), before, "table untouched");
        let u = a.schema.find_table("users").unwrap();
        assert_eq!(a.schema.tables[u].columns[2].description.as_deref(), Some("Primary contact address"));
    }

    #[test]
    fn removing_a_column_asks_first_and_referenced_columns_are_protected() {
        let mut a = app_copy("edit_drop");
        seed(&a);
        open_editor(&mut a, "organization");
        draft(&mut a).cols.remove(0); // id is referenced by users and projects
        key(&mut a, KeyCode::Enter);
        assert!(draft(&mut a).error.as_ref().unwrap().contains("referenced by"));
        key(&mut a, KeyCode::Esc);
        key(&mut a, KeyCode::Esc);

        open_editor(&mut a, "organization");
        draft(&mut a).cols.remove(2); // created_at: not referenced
        key(&mut a, KeyCode::Enter);
        assert!(draft(&mut a).error.as_ref().unwrap().contains("removes created_at"), "asks first");
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Normal), "{:?}", a.status);
        assert_eq!(query(&a, "SELECT * FROM organization ORDER BY id"), ["Integer(1)|Text(\"Acme\")", "Integer(2)|Text(\"Globex\")"]);
    }

    #[test]
    fn broken_references_roll_everything_back() {
        let mut a = app_copy("edit_fk");
        seed(&a);
        rusqlite::Connection::open(&a.cfg.db_path).unwrap().execute_batch("INSERT INTO tags (id, label) VALUES (5, 'x')").unwrap();
        let before = query(&a, "SELECT sql FROM sqlite_master ORDER BY name");
        open_editor(&mut a, "tags");
        let orgs = a.schema.find_table("organization").unwrap();
        // tag 5 would point at organization 5, which doesn't exist
        draft(&mut a).cols[0].refs = Some(RefTarget::Table(ColRef { table: orgs, column: 0 }));
        key(&mut a, KeyCode::Enter);
        let err = draft(&mut a).error.clone().unwrap();
        assert!(err.contains("nothing was changed") && err.contains("point to rows that don't exist"), "{err}");
        assert_eq!(query(&a, "SELECT sql FROM sqlite_master ORDER BY name"), before);
        assert_eq!(query(&a, "SELECT id FROM tags"), ["Integer(5)"]);
    }

    #[test]
    fn features_the_form_cannot_show_need_confirmation() {
        let mut a = app_copy("edit_lost");
        rusqlite::Connection::open(&a.cfg.db_path).unwrap()
            .execute_batch("CREATE TABLE stock (id INTEGER PRIMARY KEY, qty INT, twice INT AS (qty * 2))").unwrap();
        a.reload_schema(None, None).unwrap();
        open_editor(&mut a, "stock");
        draft(&mut a).cols.push(DraftCol { name: "sku".into(), ty: "TEXT".into(), ..Default::default() });
        key(&mut a, KeyCode::Enter);
        assert!(draft(&mut a).error.as_ref().unwrap().contains("generated columns"));
        key(&mut a, KeyCode::Left); // any other key goes back
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Design(_)), "still asks");
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Normal));
        assert!(query(&a, "SELECT sql FROM sqlite_master WHERE name = 'stock'")[0].contains("sku"));
    }

    #[test]
    fn sql_export_has_no_needless_quotes() {
        let mut a = app_copy("export_quotes");
        key(&mut a, KeyCode::Char('t'));
        typ(&mut a, "orders");
        key(&mut a, KeyCode::Down); // description
        key(&mut a, KeyCode::Down); // colour
        key(&mut a, KeyCode::Down); // id row
        key(&mut a, KeyCode::Down); // new row
        typ(&mut a, "order"); // a keyword: must stay quoted
        key(&mut a, KeyCode::Enter);
        open_editor(&mut a, "tags");
        draft(&mut a).name = "labels".into(); // SQLite quotes the new name everywhere it rewrites it
        key(&mut a, KeyCode::Enter);
        key(&mut a, KeyCode::Char('x'));
        key(&mut a, KeyCode::Char('s'));
        let sql = std::fs::read_to_string("/tmp/erd_test_export_quotes.sql").unwrap();
        assert!(sql.contains("CREATE TABLE orders (\n    id INTEGER PRIMARY KEY,\n    \"order\" TEXT\n);"), "{sql}");
        assert!(sql.contains("REFERENCES labels(id)"), "{sql}");
        assert!(!sql.contains("\"labels\"") && !sql.contains("\"id\""), "{sql}");
        let fresh = "/tmp/erd_test_export_quotes_fresh.db";
        let _ = std::fs::remove_file(fresh);
        rusqlite::Connection::open(fresh).unwrap().execute_batch(&sql).unwrap();
    }

    #[test]
    fn editing_keeps_checks_collations_actions_and_manages_indexes() {
        let mut a = app_copy("edit_extras");
        rusqlite::Connection::open(&a.cfg.db_path).unwrap().execute_batch(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                org_id INTEGER REFERENCES organization(id) ON DELETE CASCADE,
                sku TEXT COLLATE NOCASE NOT NULL CHECK (length(sku) >= 3),
                qty INT DEFAULT 0,
                UNIQUE (org_id, sku),
                CHECK (qty >= 0)
             );
             CREATE INDEX items_qty ON items (qty DESC) WHERE qty > 0;
             INSERT INTO organization (id, name) VALUES (1, 'Acme');
             INSERT INTO items (org_id, sku, qty) VALUES (1, 'abc', 5);",
        ).unwrap();
        a.reload_schema(None, None).unwrap();
        open_editor(&mut a, "items");
        {
            let d = draft(&mut a);
            assert!(d.editing.as_ref().unwrap().lost.is_empty(), "{:?}", d.editing.as_ref().unwrap().lost);
            assert!(d.cols[0].autoinc);
            assert_eq!(d.cols[1].on_delete, "CASCADE");
            assert_eq!((d.cols[2].collate.as_str(), d.cols[2].check.as_str()), ("NOCASE", "length(sku) >= 3"));
            assert_eq!((d.table_unique.as_str(), d.table_check.as_str()), ("org_id, sku", "qty >= 0"));
            assert_eq!(d.indexes.len(), 1);
            assert_eq!((d.indexes[0].cols.as_str(), d.indexes[0].where_.as_str()), ("qty DESC", "qty > 0"));
            // rename a column used in checks, unique and the index; add an index
            d.cols[3].name = "quantity".into();
            d.indexes.push(crate::designer::DraftIndex { cols: "sku".into(), unique: false, ..Default::default() });
        }
        draw(&mut a, 170, 50);
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Normal), "{:?}", a.status);
        let sql = query(&a, "SELECT sql FROM sqlite_master WHERE name = 'items'")[0].clone();
        // a rename alone is done in place; SQLite quotes the new name itself
        let plain = sql.replace(['"', '\\'], "");
        for part in ["AUTOINCREMENT", "ON DELETE CASCADE", "COLLATE NOCASE", "CHECK (length(sku) >= 3)", "UNIQUE (org_id, sku)", "CHECK (quantity >= 0)"] {
            assert!(plain.contains(part), "{part}: {sql}");
        }
        let ix = query(&a, "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'items' AND sql IS NOT NULL ORDER BY name");
        assert_eq!(ix.len(), 2, "{ix:?}");
        assert!(ix.iter().any(|i| i.contains("quantity DESC") && i.contains("WHERE quantity > 0")), "{ix:?}");
        assert_eq!(query(&a, "SELECT sku, quantity FROM items"), ["Text(\"abc\")|Integer(5)"]);

        // only an index changed: no rebuild needed
        open_editor(&mut a, "items");
        draft(&mut a).indexes.pop();
        key(&mut a, KeyCode::Enter);
        assert_eq!(query(&a, "SELECT sql FROM sqlite_master WHERE name = 'items'")[0], sql, "table untouched");
        assert_eq!(query(&a, "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'items' AND sql IS NOT NULL").len(), 1);
    }

    #[test]
    fn menu_colour_duplicate_and_delete() {
        let mut a = app_copy("menu");
        seed(&a);
        a.reload_schema(None, None).unwrap();
        draw(&mut a, 160, 45);
        let users = a.schema.find_table("users").unwrap();
        let (x, y, _, _) = a.canvas.table_rect[users].unwrap();
        let (sx, sy) = screen(&a, x + 3, y);
        mouse(&mut a, MouseEventKind::Down(MouseButton::Right), sx, sy); // right-click opens the menu
        assert!(matches!(a.mode, Mode::Menu(_)));
        key(&mut a, KeyCode::Char('c'));
        key(&mut a, KeyCode::Char('g')); // green
        assert_eq!(crate::schema::load_colors(&a.cfg.db_path).unwrap().get("users").map(|s| s.as_str()), Some("green"));
        draw(&mut a, 160, 45);

        a.select(a.resolve("organization").ok());
        key(&mut a, KeyCode::Char('m'));
        key(&mut a, KeyCode::Char('r')); // duplicate with rows
        assert!(matches!(a.mode, Mode::Prompt(_)));
        draw(&mut a, 160, 45);
        key(&mut a, KeyCode::Enter); // default name
        let copy = a.schema.find_table("organization_copy").expect("copy made");
        assert_eq!(a.schema.tables[copy].row_count, Some(2));
        assert_eq!(a.schema.tables[copy].description.as_deref(), a.schema.tables[a.schema.find_table("organization").unwrap()].description.as_deref());

        key(&mut a, KeyCode::Delete); // the copy is selected
        assert!(matches!(a.mode, Mode::Confirm(_)));
        key(&mut a, KeyCode::Char('n'));
        assert!(a.schema.find_table("organization_copy").is_some(), "cancelled");
        key(&mut a, KeyCode::Delete);
        key(&mut a, KeyCode::Char('y'));
        assert!(a.schema.find_table("organization_copy").is_none());
        assert!(query(&a, "SELECT * FROM _descriptions WHERE table_name = 'organization_copy'").is_empty());
    }

    #[test]
    fn lint_finds_problems_and_jumps_to_them() {
        let mut a = app_copy("lint");
        seed(&a);
        rusqlite::Connection::open(&a.cfg.db_path).unwrap()
            .execute_batch("PRAGMA foreign_keys = OFF; INSERT INTO users (id, org_id, email) VALUES (12, 99, 'ghost@x'); CREATE TABLE loose (a, b);").unwrap();
        a.reload_schema(None, None).unwrap();
        assert!(a.lint_counts.0 >= 1, "orphan row found: {:?}", a.lint_counts);
        key(&mut a, KeyCode::Char('!'));
        let Mode::Lint(v) = &a.mode else { panic!() };
        let msgs: Vec<&str> = v.issues.iter().map(|i| i.message.as_str()).collect();
        assert!(msgs[0].contains("users point to rows that don't exist in organization"), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("loose has no primary key")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("users.org_id has no index")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("loose.a has no type")), "{msgs:?}");
        draw(&mut a, 120, 30);
        key(&mut a, KeyCode::Enter);
        assert_eq!(a.sel, a.resolve("users").ok());
    }

    #[test]
    fn console_runs_read_only_queries() {
        let mut a = app_copy("console");
        seed(&a);
        a.select(a.resolve("organization").ok());
        key(&mut a, KeyCode::Char(':'));
        assert!(matches!(a.mode, Mode::Console));
        key(&mut a, KeyCode::Enter); // prefilled: SELECT * FROM organization LIMIT 100
        draw(&mut a, 100, 30);
        a.console.set_query("DELETE FROM organization");
        key(&mut a, KeyCode::Enter);
        draw(&mut a, 100, 30);
        assert_eq!(query(&a, "SELECT COUNT(*) FROM organization"), ["Integer(2)"], "nothing deleted");
        key(&mut a, KeyCode::Up); // history
        key(&mut a, KeyCode::Esc);
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn reloads_when_the_file_changes() {
        let mut a = app_copy("reload");
        a.select(a.resolve("users.email").ok());
        std::thread::sleep(std::time::Duration::from_millis(20));
        rusqlite::Connection::open(&a.cfg.db_path).unwrap().execute_batch("CREATE TABLE audit (id INTEGER PRIMARY KEY, what TEXT)").unwrap();
        a.check_disk();
        assert!(a.schema.find_table("audit").is_some());
        assert_eq!(a.sel, a.resolve("users.email").ok(), "selection kept");
        assert!(a.status.as_ref().unwrap().0.contains("changed on disk"));
        // our own writes don't count as outside changes
        a.status = None;
        a.select(a.resolve("audit").ok());
        a.set_color(a.schema.find_table("audit").unwrap(), Some("blue"));
        a.status = None;
        a.check_disk();
        assert!(a.status.is_none());
    }

    #[test]
    fn colour_is_set_in_the_table_form() {
        let mut a = app_copy("form_colour");
        key(&mut a, KeyCode::Char('t'));
        typ(&mut a, "things");
        match &mut a.mode {
            Mode::Design(d) => d.color = "cyan",
            _ => panic!(),
        }
        draw(&mut a, 170, 45);
        key(&mut a, KeyCode::Enter);
        assert_eq!(crate::schema::load_colors(&a.cfg.db_path).unwrap().get("things").map(|s| s.as_str()), Some("cyan"));
        // the edit form starts with it; the default colour stores nothing
        open_editor(&mut a, "things");
        assert_eq!(draft(&mut a).color, "cyan");
        draft(&mut a).color = "white";
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Normal), "{:?}", a.status);
        assert!(!crate::schema::load_colors(&a.cfg.db_path).unwrap().contains_key("things"));
        open_editor(&mut a, "users");
        assert_eq!(draft(&mut a).color, "white", "no colour = the first one");
        // hovering a field shows its hint
        draw(&mut a, 170, 45);
        let buf_has = |a: &mut App, text: &str| {
            let mut term = Terminal::new(TestBackend::new(170, 45)).unwrap();
            term.draw(|f| a.draw(f)).unwrap();
            let buf = term.backend().buffer().clone();
            (0..45).any(|y| (0..170).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().contains(text))
        };
        assert!(!buf_has(&mut a, "every row needs a value"));
        let nn = match &a.mode { Mode::Design(d) => d.rect_of_for_test(crate::designer::Field::Col(1, crate::designer::ColField::NotNull)), _ => None }.unwrap();
        mouse(&mut a, MouseEventKind::Moved, nn.x as usize, nn.y as usize);
        assert!(buf_has(&mut a, "every row needs a value"));
    }
}
