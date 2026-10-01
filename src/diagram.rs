//! Turns a schema into a character canvas: table boxes placed on a grid,
//! foreign-key lines routed orthogonally through the gaps between boxes.
//!
//! Layout: tables are layered by FK depth (referenced tables on the left,
//! referencing tables on the right). Vertical "gutters" separate grid columns
//! and horizontal "channels" separate grid rows. Every line runs only through
//! gutters and channels, so it never crosses a table box.

use crate::schema::{ColRef, Column, Schema, Table};
use ratatui::style::{Color, Modifier, Style};
use std::collections::{HashMap, HashSet};

pub const PK_COLOR: Color = Color::Yellow;
pub const FK_COLOR: Color = Color::Cyan;
pub const HL_COLOR: Color = Color::LightMagenta;
pub const MOVE_COLOR: Color = Color::LightYellow;
const LINE_COLOR: Color = Color::DarkGray;
const DIM_COLOR: Color = Color::DarkGray;

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Column(ColRef),
    Table(usize),
}

// ---------------------------------------------------------------- highlight

#[derive(Default)]
pub struct Highlight {
    pub edges: HashSet<usize>,
    pub columns: HashSet<ColRef>,
}

/// Which FK edges and columns light up for a selection.
/// Column: incoming references + the outgoing chain (followed transitively).
/// Table: every relation touching the table.
pub fn highlight(schema: &Schema, sel: Option<Selection>) -> Highlight {
    let mut h = Highlight::default();
    match sel {
        None => {}
        Some(Selection::Table(t)) => {
            for (i, fk) in schema.fks.iter().enumerate() {
                if fk.from.table == t || fk.to.table == t {
                    h.edges.insert(i);
                    h.columns.insert(fk.from);
                    h.columns.insert(fk.to);
                }
            }
        }
        Some(Selection::Column(c)) => {
            h.columns.insert(c);
            for (i, fk) in schema.fks.iter().enumerate() {
                if fk.to == c {
                    h.edges.insert(i);
                    h.columns.insert(fk.from);
                }
            }
            let mut stack = vec![c];
            let mut seen = HashSet::new();
            while let Some(cur) = stack.pop() {
                if !seen.insert(cur) {
                    continue;
                }
                for (i, fk) in schema.fks.iter().enumerate() {
                    if fk.from == cur {
                        h.edges.insert(i);
                        h.columns.insert(fk.to);
                        stack.push(fk.to);
                    }
                }
            }
        }
    }
    h
}

// --------------------------------------------------------------------- grid

#[derive(Clone, PartialEq, Debug)]
pub struct Grid {
    /// (grid column, grid row) per table; None = not shown
    pub pos: Vec<Option<(usize, usize)>>,
    pub ncols: usize,
    pub nrows: usize,
}

impl Grid {
    pub fn from_positions(pos: Vec<Option<(usize, usize)>>) -> Grid {
        let ncols = pos.iter().flatten().map(|p| p.0 + 1).max().unwrap_or(0);
        let nrows = pos.iter().flatten().map(|p| p.1 + 1).max().unwrap_or(0);
        Grid { pos, ncols, nrows }
    }

    /// Keep only some tables and squeeze out empty grid columns and rows.
    pub fn compacted(&self, keep: impl Fn(usize) -> bool) -> Grid {
        let pos: Vec<Option<(usize, usize)>> =
            self.pos.iter().enumerate().map(|(i, p)| if keep(i) { *p } else { None }).collect();
        let mut cols: Vec<usize> = pos.iter().flatten().map(|p| p.0).collect();
        let mut rows: Vec<usize> = pos.iter().flatten().map(|p| p.1).collect();
        cols.sort_unstable();
        cols.dedup();
        rows.sort_unstable();
        rows.dedup();
        let pos = pos
            .into_iter()
            .map(|p| p.map(|(c, r)| (cols.binary_search(&c).unwrap(), rows.binary_search(&r).unwrap())))
            .collect();
        Grid::from_positions(pos)
    }

    pub fn occupant(&self, c: usize, r: usize) -> Option<usize> {
        self.pos.iter().position(|p| *p == Some((c, r)))
    }
}

/// Automatic layout with saved positions applied on top. Tables without a
/// saved position keep their automatic spot, moved down if it's taken.
pub fn with_saved(schema: &Schema, auto: &Grid, saved: &HashMap<String, (usize, usize)>) -> Grid {
    let n = schema.tables.len();
    let mut pos = vec![None; n];
    let mut used = HashSet::new();
    for (slot, table) in pos.iter_mut().zip(&schema.tables) {
        if let Some(&p) = saved.get(&table.name.to_lowercase()) {
            if used.insert(p) {
                *slot = Some(p);
            }
        }
    }
    for (t, slot) in pos.iter_mut().enumerate() {
        if slot.is_none() {
            let (c, mut r) = auto.pos[t].unwrap_or((0, 0));
            while used.contains(&(c, r)) {
                r += 1;
            }
            used.insert((c, r));
            *slot = Some((c, r));
        }
    }
    Grid::from_positions(pos).compacted(|_| true)
}

/// Columns still shown when a table is collapsed.
pub fn is_key_column(c: &Column) -> bool {
    c.is_pk() || c.fk || c.referenced
}

pub fn compute_grid(schema: &Schema) -> Grid {
    let n = schema.tables.len();
    if n == 0 {
        return Grid::from_positions(vec![]);
    }

    // FK depth: tables referencing nothing are 0, others 1 + depth of target.
    let mut depth = vec![0usize; n];
    for _ in 0..n {
        let mut changed = false;
        for fk in &schema.fks {
            let (a, b) = (fk.from.table, fk.to.table);
            if a != b && depth[a] < depth[b] + 1 && depth[b] + 1 < n {
                depth[a] = depth[b] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut levels = depth.clone();
    levels.sort_unstable();
    levels.dedup();

    let per_col = ((n as f64).sqrt().ceil() as usize).max(1);
    let mut pos = vec![(0, 0); n];
    let mut order = vec![f64::MAX; n];
    let mut placed = 0usize;
    let (mut col, mut nrows) = (0, 0);

    for lvl in levels {
        let mut members: Vec<usize> = (0..n).filter(|&t| depth[t] == lvl).collect();
        // barycenter: sit close to the tables you reference
        let key = |t: usize| -> f64 {
            let refs: Vec<f64> = schema
                .fks
                .iter()
                .filter(|f| f.from.table == t && order[f.to.table] < f64::MAX)
                .map(|f| order[f.to.table])
                .collect();
            if refs.is_empty() {
                f64::MAX
            } else {
                refs.iter().sum::<f64>() / refs.len() as f64
            }
        };
        let keys: Vec<f64> = (0..n).map(key).collect();
        members.sort_by(|&a, &b| {
            keys[a]
                .partial_cmp(&keys[b])
                .unwrap()
                .then_with(|| schema.tables[a].name.to_lowercase().cmp(&schema.tables[b].name.to_lowercase()))
        });
        for chunk in members.chunks(per_col) {
            for (r, &t) in chunk.iter().enumerate() {
                pos[t] = (col, r);
                order[t] = placed as f64;
                placed += 1;
            }
            nrows = nrows.max(chunk.len());
            col += 1;
        }
    }
    let _ = (col, nrows);
    Grid::from_positions(pos.into_iter().map(Some).collect())
}

// ------------------------------------------------------------------ measure

#[derive(Clone, Copy)]
pub struct Measure {
    pub w: usize,
    name_w: usize,
    type_w: usize,
    desc_w: usize,
}

impl Measure {
    const NAME_X: usize = 5; // "│ PK name"
    fn type_x(&self) -> usize {
        Self::NAME_X + self.name_w + 2
    }
    fn desc_x(&self) -> usize {
        if self.type_w > 0 {
            self.type_x() + self.type_w + 2
        } else {
            self.type_x()
        }
    }
}

/// "1,234 rows" for the bottom edge of a table box.
pub fn rows_label(n: Option<i64>) -> String {
    match n {
        None => String::new(),
        Some(1) => " 1 row ".into(),
        Some(n) => {
            let digits = n.to_string();
            let mut out = String::new();
            for (i, ch) in digits.chars().enumerate() {
                if i > 0 && (digits.len() - i) % 3 == 0 {
                    out.push(',');
                }
                out.push(ch);
            }
            format!(" {out} rows ")
        }
    }
}

/// Colours a table can be given (name as stored, colour on screen). The
/// first is the default. Magenta is left out: it marks the selection.
pub const TABLE_COLORS: [(&str, Color); 7] = [
    ("white", Color::White),
    ("red", Color::LightRed),
    ("green", Color::LightGreen),
    ("yellow", Color::Yellow),
    ("blue", Color::LightBlue),
    ("cyan", Color::Cyan),
    ("dark gray", Color::DarkGray),
];

pub fn color_named(name: &str) -> Option<Color> {
    TABLE_COLORS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, c)| *c)
}

pub fn measure(t: &Table, desc_max: usize) -> Measure {
    let len = |s: &str| s.chars().count();
    let name_w = t.columns.iter().map(|c| len(&c.name)).max().unwrap_or(0);
    let type_w = t.columns.iter().map(|c| len(&c.data_type)).max().unwrap_or(0);
    let desc_w = t
        .columns
        .iter()
        .filter_map(|c| c.description.as_deref())
        .map(len)
        .max()
        .unwrap_or(0)
        .min(desc_max);
    let mut m = Measure { w: 0, name_w, type_w, desc_w };
    let content = if desc_w > 0 { m.desc_x() + desc_w } else if type_w > 0 { m.type_x() + type_w } else { Measure::NAME_X + name_w };
    // content + trailing space + right border; title needs "┌─ name ─┐"
    // room for the title, and at the bottom for "+N hidden" and the row count
    m.w = (content + 2).max(len(&t.name) + 6).max(rows_label(t.row_count).chars().count() + 16);
    m
}

struct BoxLayout {
    h: usize,
    tdesc: Vec<String>,
    /// per column: (row offset inside the box, description lines); None = hidden
    rows: Vec<Option<(usize, Vec<String>)>>,
    hidden: usize,
}

fn box_layout(t: &Table, ti: usize, m: &Measure, sel: Option<Selection>, collapsed: bool) -> BoxLayout {
    let text_w = m.w - 4;
    let mut off = 1;
    let tdesc = match &t.description {
        Some(d) if sel == Some(Selection::Table(ti)) => wrap(d, text_w),
        Some(d) => vec![truncate(d, text_w)],
        None => vec![],
    };
    if !tdesc.is_empty() {
        off += tdesc.len() + 1;
    }
    let mut rows = Vec::with_capacity(t.columns.len());
    let mut hidden = 0;
    for (ci, c) in t.columns.iter().enumerate() {
        let expanded = sel == Some(Selection::Column(ColRef { table: ti, column: ci }));
        if collapsed && !expanded && !is_key_column(c) {
            rows.push(None);
            hidden += 1;
            continue;
        }
        let lines = match &c.description {
            Some(d) if m.desc_w > 0 && expanded => wrap(d, m.desc_w),
            Some(d) if m.desc_w > 0 => vec![truncate(d, m.desc_w)],
            _ => vec![String::new()],
        };
        let n = lines.len().max(1);
        rows.push(Some((off, lines)));
        off += n;
    }
    BoxLayout { h: off + 1, tdesc, rows, hidden }
}

// ------------------------------------------------------------------- canvas

#[derive(Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

pub struct Hit {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    target: Selection,
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    cells: Vec<Cell>,
    hits: Vec<Hit>,
    /// (x, y, w, h) of each shown table box
    pub table_rect: Vec<Option<(usize, usize, usize, usize)>>,
    /// canvas y of each shown column row
    pub col_y: HashMap<ColRef, usize>,
    pub map: GridMap,
}

/// Where the grid cells are on the canvas, for mapping points to cells.
#[derive(Clone, Default)]
pub struct GridMap {
    /// x where gutter g starts (ncols + 1 entries) / y where channel r starts
    gx: Vec<usize>,
    ch_y: Vec<usize>,
}

impl GridMap {
    /// Grid cell under a canvas point; may be one past the last column/row.
    pub fn cell_at(&self, x: usize, y: usize) -> (usize, usize) {
        let c = self.gx.iter().rposition(|&g| g <= x).unwrap_or(0);
        let r = self.ch_y.iter().rposition(|&v| v <= y).unwrap_or(0);
        (c, r)
    }
}

impl Canvas {
    fn new(w: usize, h: usize, ntables: usize) -> Self {
        Canvas {
            w,
            h,
            cells: vec![Cell { ch: ' ', style: Style::default() }; w * h],
            hits: vec![],
            table_rect: vec![None; ntables],
            col_y: HashMap::new(),
            map: GridMap::default(),
        }
    }

    fn put(&mut self, x: usize, y: usize, ch: char, style: Style) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x] = Cell { ch, style };
        }
    }

    fn put_str(&mut self, x: usize, y: usize, s: &str, style: Style) {
        for (i, ch) in s.chars().enumerate() {
            self.put(x + i, y, ch, style);
        }
    }

    pub fn get(&self, x: usize, y: usize) -> Option<&Cell> {
        (x < self.w && y < self.h).then(|| &self.cells[y * self.w + x])
    }

    pub fn hit(&self, x: usize, y: usize) -> Option<Selection> {
        self.hits
            .iter()
            .find(|h| x >= h.x0 && x <= h.x1 && y >= h.y0 && y <= h.y1)
            .map(|h| h.target)
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for y in 0..self.h {
            let line: String = self.cells[y * self.w..(y + 1) * self.w].iter().map(|c| c.ch).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

struct Plan {
    src_side: Side,
    dst_side: Side,
    g_src: usize,
    lane_src: usize,
    g_dst: usize,
    lane_dst: usize,
    /// (channel index, track) when the line has to travel across columns
    channel: Option<(usize, usize)>,
}

pub fn build(
    schema: &Schema,
    grid: &Grid,
    measures: &[Measure],
    sel: Option<Selection>,
    collapsed: &HashSet<usize>,
    moving: Option<usize>,
    colors: &[Option<Color>],
) -> Canvas {
    let n = schema.tables.len();
    let hl = highlight(schema, sel);
    let layouts: Vec<Option<BoxLayout>> = (0..n)
        .map(|t| {
            grid.pos[t].map(|_| box_layout(&schema.tables[t], t, &measures[t], sel, collapsed.contains(&t)))
        })
        .collect();
    let lay = |t: usize| layouts[t].as_ref().unwrap();
    let at = |t: usize| grid.pos[t].unwrap();

    let mut col_w = vec![0; grid.ncols];
    let mut row_h = vec![0; grid.nrows];
    for t in (0..n).filter(|&t| grid.pos[t].is_some()) {
        let (c, r) = at(t);
        col_w[c] = col_w[c].max(measures[t].w);
        row_h[r] = row_h[r].max(lay(t).h);
    }
    // edges whose both ends are shown
    let edges: Vec<usize> = (0..schema.fks.len())
        .filter(|&i| {
            let fk = &schema.fks[i];
            grid.pos[fk.from.table].is_some()
                && grid.pos[fk.to.table].is_some()
                && lay(fk.from.table).rows[fk.from.column].is_some()
                && lay(fk.to.table).rows[fk.to.column].is_some()
        })
        .collect();

    // --- plan routes & allocate lanes/tracks
    let mut lanes = vec![0usize; grid.ncols + 1];
    let mut tracks = vec![0usize; grid.nrows + 1];
    // lines into the same target column share their last lane/track (fan-in)
    let mut shared_lane: HashMap<(usize, ColRef), usize> = HashMap::new();
    let mut shared_track: HashMap<(usize, ColRef), usize> = HashMap::new();
    let alloc = |counts: &mut Vec<usize>, i: usize| {
        counts[i] += 1;
        counts[i] - 1
    };

    let mut plans = Vec::with_capacity(edges.len());
    for fk in edges.iter().map(|&i| &schema.fks[i]) {
        let (ca, _) = at(fk.from.table);
        let (cb, rb) = at(fk.to.table);
        let (src_side, dst_side, g_src, g_dst, direct) = if ca == cb {
            (Side::Right, Side::Right, ca + 1, ca + 1, true)
        } else if cb == ca + 1 {
            (Side::Right, Side::Left, ca + 1, ca + 1, true)
        } else if ca == cb + 1 {
            (Side::Left, Side::Right, ca, ca, true)
        } else if cb > ca {
            (Side::Right, Side::Left, ca + 1, cb, false)
        } else {
            (Side::Left, Side::Right, ca, cb + 1, false)
        };
        let lane_dst = *shared_lane
            .entry((g_dst, fk.to))
            .or_insert_with(|| alloc(&mut lanes, g_dst));
        let (lane_src, channel) = if direct {
            (lane_dst, None)
        } else {
            let tr = *shared_track
                .entry((rb, fk.to))
                .or_insert_with(|| alloc(&mut tracks, rb));
            (alloc(&mut lanes, g_src), Some((rb, tr)))
        };
        plans.push(Plan { src_side, dst_side, g_src, lane_src, g_dst, lane_dst, channel });
    }

    // --- coordinates
    let gw: Vec<usize> = (0..=grid.ncols)
        .map(|g| match lanes[g] {
            0 if g == 0 || g == grid.ncols => 2,
            0 => 4,
            l => 2 * l + 3,
        })
        .collect();
    let chh: Vec<usize> = (0..=grid.nrows)
        .map(|r| if tracks[r] == 0 { 1 } else { tracks[r] + 2 })
        .collect();

    let mut gx = vec![0; grid.ncols + 1];
    let mut col_x = vec![0; grid.ncols];
    let mut x = 0;
    for c in 0..grid.ncols {
        gx[c] = x;
        col_x[c] = x + gw[c];
        x = col_x[c] + col_w[c];
    }
    gx[grid.ncols] = x;
    let width = x + gw[grid.ncols];

    let mut ch_y = vec![0; grid.nrows + 1];
    let mut row_y = vec![0; grid.nrows];
    let mut y = 0;
    for r in 0..grid.nrows {
        ch_y[r] = y;
        row_y[r] = y + chh[r];
        y = row_y[r] + row_h[r];
    }
    ch_y[grid.nrows] = y;
    let height = y + chh[grid.nrows];

    let lane_x = |g: usize, k: usize| gx[g] + 2 + 2 * k;
    let track_y = |r: usize, k: usize| ch_y[r] + 1 + k;
    let border_x = |t: usize, side: Side| {
        let (c, _) = at(t);
        match side {
            Side::Left => col_x[c],
            Side::Right => col_x[c] + measures[t].w - 1,
        }
    };
    let col_row_y = |c: ColRef| row_y[at(c.table).1] + lay(c.table).rows[c.column].as_ref().unwrap().0;

    // --- rasterise lines into direction masks
    let mut cv = Canvas::new(width, height, n);
    cv.map = GridMap { gx: gx.clone(), ch_y: ch_y.clone() };
    let mut normal = vec![0u8; width * height];
    let mut lit = vec![0u8; width * height];
    let mut marks: Vec<(usize, usize, char, bool)> = Vec::new();

    for (&i, p) in edges.iter().zip(&plans) {
        let fk = &schema.fks[i];
        let sy = col_row_y(fk.from);
        let ty = col_row_y(fk.to);
        let sx = border_x(fk.from.table, p.src_side);
        let dx = border_x(fk.to.table, p.dst_side);
        let l1 = lane_x(p.g_src, p.lane_src);
        let pts = match p.channel {
            None => vec![(sx, sy), (l1, sy), (l1, ty), (dx, ty)],
            Some((r, k)) => {
                let cy = track_y(r, k);
                let l2 = lane_x(p.g_dst, p.lane_dst);
                vec![(sx, sy), (l1, sy), (l1, cy), (l2, cy), (l2, ty), (dx, ty)]
            }
        };
        let is_hl = hl.edges.contains(&i);
        let mask = if is_hl { &mut lit } else { &mut normal };
        for w in pts.windows(2) {
            segment(mask, width, w[0], w[1]);
        }
        let join = |s: Side| if s == Side::Right { '├' } else { '┤' };
        marks.push((sx, sy, join(p.src_side), is_hl));
        marks.push((dx, ty, join(p.dst_side), is_hl));
        match p.dst_side {
            Side::Left => marks.push((dx - 1, ty, '▶', is_hl)),
            Side::Right => marks.push((dx + 1, ty, '◀', is_hl)),
        }
    }

    let line_style = Style::default().fg(LINE_COLOR);
    let hl_style = Style::default().fg(HL_COLOR).add_modifier(Modifier::BOLD);
    for idx in 0..width * height {
        let (m, st) = if lit[idx] != 0 { (lit[idx], hl_style) } else { (normal[idx], line_style) };
        if m != 0 {
            cv.cells[idx] = Cell { ch: glyph(m), style: st };
        }
    }

    // --- boxes on top of lines
    for t in (0..n).filter(|&t| grid.pos[t].is_some()) {
        let (c, r) = at(t);
        let mv = moving == Some(t);
        let color = colors.get(t).copied().flatten();
        draw_box(&mut cv, &schema.tables[t], t, &measures[t], lay(t), col_x[c], row_y[r], sel, &hl, mv, color);
    }

    // --- joins & arrowheads last (highlighted ones win)
    marks.sort_by_key(|m| m.3);
    for (x, y, ch, is_hl) in marks {
        cv.put(x, y, ch, if is_hl { hl_style } else { line_style });
    }
    cv
}

fn segment(mask: &mut [u8], width: usize, (x1, y1): (usize, usize), (x2, y2): (usize, usize)) {
    if y1 == y2 {
        let (a, b) = (x1.min(x2), x1.max(x2));
        for x in a..=b {
            if x > a {
                mask[y1 * width + x] |= LEFT;
            }
            if x < b {
                mask[y1 * width + x] |= RIGHT;
            }
        }
    } else {
        let (a, b) = (y1.min(y2), y1.max(y2));
        for y in a..=b {
            if y > a {
                mask[y * width + x1] |= UP;
            }
            if y < b {
                mask[y * width + x1] |= DOWN;
            }
        }
    }
}

fn glyph(m: u8) -> char {
    let (u, d, l, r) = (m & UP != 0, m & DOWN != 0, m & LEFT != 0, m & RIGHT != 0);
    match (u, d, l, r) {
        (true, true, true, true) => '┼',
        (true, true, false, true) => '├',
        (true, true, true, false) => '┤',
        (false, true, true, true) => '┬',
        (true, false, true, true) => '┴',
        (false, true, false, true) => '╭',
        (false, true, true, false) => '╮',
        (true, false, false, true) => '╰',
        (true, false, true, false) => '╯',
        (_, _, false, false) => '│',
        _ => '─',
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_box(
    cv: &mut Canvas,
    t: &Table,
    ti: usize,
    m: &Measure,
    lay: &BoxLayout,
    x: usize,
    y: usize,
    sel: Option<Selection>,
    hl: &Highlight,
    moving: bool,
    color: Option<Color>,
) {
    let (w, h) = (m.w, lay.h);
    let table_sel = sel == Some(Selection::Table(ti));
    let border = match (moving, table_sel, color) {
        (true, _, _) => Style::default().fg(MOVE_COLOR).add_modifier(Modifier::BOLD),
        (false, true, _) => Style::default().fg(HL_COLOR),
        (_, _, Some(c)) => Style::default().fg(c),
        _ => Style::default().fg(TABLE_COLORS[0].1),
    };
    // a selected or dragged table gets a double border
    let double = moving || table_sel;
    let [hz, vt, tl, tr, bl, br] = if double {
        ['═', '║', '╔', '╗', '╚', '╝']
    } else {
        ['─', '│', '┌', '┐', '└', '┘']
    };

    for i in 1..w - 1 {
        cv.put(x + i, y, hz, border);
        cv.put(x + i, y + h - 1, hz, border);
    }
    for j in 1..h - 1 {
        cv.put(x, y + j, vt, border);
        cv.put(x + w - 1, y + j, vt, border);
    }
    cv.put(x, y, tl, border);
    cv.put(x + w - 1, y, tr, border);
    cv.put(x, y + h - 1, bl, border);
    cv.put(x + w - 1, y + h - 1, br, border);
    let title = match (moving, table_sel, color) {
        (true, _, _) => Style::default().fg(Color::Black).bg(MOVE_COLOR),
        (false, true, _) => Style::default().fg(HL_COLOR),
        (_, _, Some(c)) => Style::default().fg(c),
        _ => Style::default().fg(TABLE_COLORS[0].1),
    }
    .add_modifier(Modifier::BOLD);
    cv.put_str(x + 2, y, &format!(" {} ", t.name), title);

    let mut row = y + 1;
    if !lay.tdesc.is_empty() {
        let st = Style::default().fg(DIM_COLOR).add_modifier(Modifier::ITALIC);
        for line in &lay.tdesc {
            cv.put_str(x + 2, row, line, st);
            row += 1;
        }
        let (l, r) = if double { ('╟', '╢') } else { ('├', '┤') };
        cv.put(x, row, l, border);
        for i in 1..w - 1 {
            cv.put(x + i, row, '─', border);
        }
        cv.put(x + w - 1, row, r, border);
    }
    cv.hits.push(Hit { x0: x, y0: y, x1: x + w - 1, y1: row.max(y), target: Selection::Table(ti) });

    if lay.hidden > 0 {
        let label = format!(" +{} hidden ", lay.hidden);
        cv.put_str(x + 2, y + h - 1, &label, Style::default().fg(DIM_COLOR));
    }
    let rows = rows_label(t.row_count);
    if !rows.is_empty() {
        let rw = rows.chars().count();
        cv.put_str(x + w - 2 - rw, y + h - 1, &rows, Style::default().fg(DIM_COLOR));
    }

    for (ci, c) in t.columns.iter().enumerate() {
        let cr = ColRef { table: ti, column: ci };
        let Some((off, lines)) = &lay.rows[ci] else { continue };
        let ry = y + off;
        let nlines = lines.len().max(1);
        let selected = sel == Some(Selection::Column(cr));
        let base = if selected { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };

        for j in 0..nlines {
            for i in 1..w - 1 {
                cv.put(x + i, ry + j, ' ', base);
            }
        }
        let pk = base.patch(Style::default().fg(PK_COLOR).add_modifier(Modifier::BOLD));
        let fk = base.patch(Style::default().fg(FK_COLOR).add_modifier(Modifier::BOLD));
        match (c.is_pk(), c.fk) {
            (true, true) => {
                cv.put(x + 2, ry, 'P', pk);
                cv.put(x + 3, ry, 'F', fk);
            }
            (true, false) => cv.put_str(x + 2, ry, "PK", pk),
            (false, true) => cv.put_str(x + 2, ry, "FK", fk),
            _ => {}
        }
        let name_style = if hl.columns.contains(&cr) && !selected {
            Style::default().fg(HL_COLOR).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else if c.is_pk() {
            Style::default().fg(PK_COLOR).add_modifier(Modifier::BOLD)
        } else if c.fk {
            Style::default().fg(FK_COLOR)
        } else {
            Style::default()
        };
        cv.put_str(x + Measure::NAME_X, ry, &c.name, base.patch(name_style));
        cv.put_str(x + m.type_x(), ry, &c.data_type, base.patch(Style::default().fg(Color::Gray)));
        let desc_style = base.patch(Style::default().fg(if selected { Color::White } else { Color::Gray }));
        for (j, line) in lines.iter().enumerate() {
            cv.put_str(x + m.desc_x(), ry + j, line, desc_style);
        }
        cv.hits.push(Hit { x0: x, y0: ry, x1: x + w - 1, y1: ry + nlines - 1, target: Selection::Column(cr) });
        cv.col_y.insert(cr, ry);
    }
    cv.table_rect[ti] = Some((x, y, w, h));
}

// -------------------------------------------------------------------- text

pub fn truncate(s: &str, w: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= w {
        flat
    } else if w == 0 {
        String::new()
    } else {
        let mut t: String = flat.chars().take(w - 1).collect();
        t.push('…');
        t
    }
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut cur = String::new();
        let mut cur_len = 0;
        for word in para.split_whitespace() {
            let mut word: Vec<char> = word.chars().collect();
            while word.len() > width {
                if cur_len > 0 {
                    lines.push(std::mem::take(&mut cur));
                    cur_len = 0;
                }
                lines.push(word[..width].iter().collect());
                word.drain(..width);
            }
            if word.is_empty() {
                continue;
            }
            if cur_len == 0 {
                cur = word.iter().collect();
                cur_len = word.len();
            } else if cur_len + 1 + word.len() <= width {
                cur.push(' ');
                cur.extend(word.iter());
                cur_len += 1 + word.len();
            } else {
                lines.push(std::mem::take(&mut cur));
                cur = word.iter().collect();
                cur_len = word.len();
            }
        }
        lines.push(cur);
    }
    while lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
