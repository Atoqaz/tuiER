//! The table form, for new tables and for editing existing ones: table
//! name and description; one row per column (name, type, PK, NOT NULL,
//! UNIQUE, default, reference, description) with details for the focused
//! column (COLLATE, CHECK, ON DELETE / ON UPDATE, AUTOINCREMENT); table
//! constraints (multi-column UNIQUE, CHECK); and the table's indexes.

use crate::diagram::{truncate, FK_COLOR, HL_COLOR, PK_COLOR, TABLE_COLORS};
use crate::schema::{ident_sql, ColRef, Schema, FK_ACTIONS, REBUILD_TMP};
use crate::sqlparse::{parse_index_sql, rename_idents, split_top_level};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColField {
    Name,
    Type,
    Pk,
    NotNull,
    Unique,
    Default,
    Refs,
    Desc,
    Collate,
    Check,
    OnDelete,
    OnUpdate,
    AutoInc,
}

/// The column grid, left to right: (field, header, base width; 0 = the rest).
const GRID: [(ColField, &str, u16); 13] = [
    (ColField::Name, "Column", 14),
    (ColField::Type, "Type", 13),
    (ColField::Pk, "PK", 3),
    (ColField::NotNull, "NN", 3),
    (ColField::Unique, "UQ", 3),
    (ColField::AutoInc, "AI", 3),
    (ColField::Default, "Default", 10),
    (ColField::Collate, "Collate", 7),
    (ColField::Check, "Check", 14),
    (ColField::Refs, "References", 18),
    (ColField::OnDelete, "On delete", 11),
    (ColField::OnUpdate, "On update", 11),
    (ColField::Desc, "Description", 0),
];

/// Collations built into SQLite ("" = the default, BINARY).
const COLLATIONS: [&str; 4] = ["", "BINARY", "NOCASE", "RTRIM"];

/// Column types offered by the type menu, by category; `Param` says
/// whether the type takes a number, like NVARCHAR(15).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Param {
    No,
    Length,
    Precision,
}

type TypeGroup = (&'static str, &'static str, &'static [(&'static str, Param, &'static str)]);

/// (category, note, types), each type with a short note on its limits or
/// use, shown in grey in the type menu.
const TYPE_GROUPS: [TypeGroup; 5] = [
    ("Integer", "SQLite: all 64-bit, size not enforced", &[
        ("INTEGER", Param::No, "±9.2e18 · PK = rowid"),
        ("BIGINT", Param::No, "±9.2e18"),
        ("SMALLINT", Param::No, "±32767 by convention"),
        ("TINYINT", Param::No, "0–255 by convention"),
        ("BOOLEAN", Param::No, "0 / 1"),
    ]),
    ("Text", "SQLite: length never enforced", &[
        ("TEXT", Param::No, "any length · the usual pick"),
        ("VARCHAR", Param::Length, "≤ n chars, documents a limit"),
        ("NVARCHAR", Param::Length, "≤ n, Unicode in other DBs"),
        ("CHAR", Param::Length, "fixed n, e.g. codes"),
        ("NCHAR", Param::Length, "fixed n, Unicode"),
        ("CLOB", Param::No, "large text, = TEXT"),
    ]),
    ("Real", "8-byte float, ~15 digits", &[
        ("REAL", Param::No, "±1.8e308"),
        ("DOUBLE", Param::No, "= REAL"),
        ("FLOAT", Param::No, "= REAL"),
    ]),
    ("Numeric", "kept as integer if exact, else REAL", &[
        ("NUMERIC", Param::No, "int or real, as fits"),
        ("DECIMAL", Param::Precision, "p digits, s decimals · not exact"),
        ("DATE", Param::No, "'YYYY-MM-DD'"),
        ("DATETIME", Param::No, "'YYYY-MM-DD HH:MM:SS'"),
    ]),
    ("Binary", "", &[("BLOB", Param::No, "raw bytes, stored as given")]),
];

impl ColField {
    fn is_checkbox(self) -> bool {
        matches!(self, ColField::Pk | ColField::NotNull | ColField::Unique | ColField::AutoInc)
    }
    fn is_choice(self) -> bool {
        matches!(self, ColField::OnDelete | ColField::OnUpdate | ColField::Collate)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdxField {
    Name,
    Cols,
    Unique,
    Where,
}

const IDX_FIELDS: [IdxField; 4] = [IdxField::Name, IdxField::Cols, IdxField::Unique, IdxField::Where];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Name,
    Desc,
    Color,
    Col(usize, ColField),
    TableUnique,
    TableCheck,
    Idx(usize, IdxField),
}

/// What a column references: an existing column, or a column of this
/// table itself (self-reference), by row index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefTarget {
    Table(ColRef),
    This(usize),
}

#[derive(Clone, Default, PartialEq, Debug)]
pub struct DraftCol {
    pub name: String,
    pub ty: String,
    pub pk: bool,
    pub not_null: bool,
    pub unique: bool,
    pub default: String,
    pub refs: Option<RefTarget>,
    pub desc: String,
    /// index of the existing column this row edits (None = new column)
    pub orig: Option<usize>,
    pub collate: String,
    pub check: String,
    pub autoinc: bool,
    pub on_delete: &'static str,
    pub on_update: &'static str,
}

const NEW_COL_TYPE: &str = "TEXT";

impl DraftCol {
    /// Nothing filled in (a row added by arrowing down, not used yet).
    pub fn is_blank(&self) -> bool {
        self.name.trim().is_empty()
            && (self.ty.trim().is_empty() || self.ty == NEW_COL_TYPE)
            && !self.pk
            && !self.not_null
            && !self.unique
            && !self.autoinc
            && self.default.trim().is_empty()
            && self.refs.is_none()
            && self.desc.trim().is_empty()
            && self.collate.trim().is_empty()
            && self.check.trim().is_empty()
            && self.on_delete.is_empty()
            && self.on_update.is_empty()
    }

}

#[derive(Clone, Default, PartialEq, Debug)]
pub struct DraftIndex {
    pub name: String,
    pub cols: String,
    pub unique: bool,
    pub where_: String,
}

impl DraftIndex {
    fn is_blank(&self) -> bool {
        self.name.trim().is_empty() && self.cols.trim().is_empty() && self.where_.trim().is_empty() && !self.unique
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PickerKind {
    Refs,
    Type,
    /// collate, on delete/update or colour: the focused field
    Choice,
    /// the columns of the focused index
    IndexCols,
}

/// A filterable list over the form (references, column types, a fixed
/// choice or index columns). For a type that takes a number, `param` asks
/// for it: (type, number text). For index columns, `parts` is the list
/// being built, in order: "name", "name DESC", "lower(email)"…
struct Picker {
    kind: PickerKind,
    query: String,
    idx: usize,
    param: Option<(&'static str, Param, String)>,
    parts: Vec<String>,
}

impl Picker {
    fn new(kind: PickerKind, query: String) -> Self {
        Picker { kind, query, idx: 0, param: None, parts: Vec::new() }
    }
}

/// The column an index part is on ("name DESC" → name), unquoted; None for
/// an expression like lower(email).
fn part_column(part: &str) -> Option<String> {
    let p = part.trim();
    let (name, rest) = match p.chars().next()? {
        q @ ('"' | '`' | '[') => {
            let close = if q == '[' { ']' } else { q };
            let end = p[1..].find(close)? + 1;
            (p[1..end].to_string(), &p[end + 1..])
        }
        _ => {
            let end = p.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(p.len());
            (p[..end].to_string(), &p[end..])
        }
    };
    (!name.is_empty() && (rest.is_empty() || rest.starts_with(char::is_whitespace))).then_some(name)
}

/// An entry of the choice list; `note` is shown in grey on the right.
struct ChoiceItem {
    label: &'static str,
    value: &'static str,
    note: &'static str,
    swatch: Option<Color>,
}

struct RefItem {
    label: String,
    ty: String,
    target: RefTarget,
}

pub enum Outcome {
    Continue,
    Cancel,
    Submit,
}

/// Editing an existing table (instead of creating one).
pub struct EditInfo {
    pub table: usize,
    pub orig_name: String,
    /// things in the table's SQL the form can't show, lost on a rebuild
    pub lost: Vec<String>,
}

/// What saving an edited table amounts to.
pub enum Change {
    Nothing,
    DescriptionsOnly,
    Apply(Plan),
}

pub struct Plan {
    pub renames: Vec<(String, String)>,
    pub added: Vec<String>,
    pub dropped: Vec<String>,
    /// CREATE TABLE (as REBUILD_TMP) and the columns to copy; None when
    /// only renames and/or indexes changed
    pub rebuild: Option<(String, Vec<String>)>,
    /// the full new set of CREATE INDEX statements; None = unchanged
    pub indexes: Option<Vec<String>>,
}

/// The form's content, to detect changes.
#[derive(Clone, PartialEq, Default)]
struct Snapshot {
    name: String,
    desc: String,
    cols: Vec<DraftCol>,
    table_unique: String,
    table_check: String,
    indexes: Vec<DraftIndex>,
    color: &'static str,
}

pub struct Draft {
    pub name: String,
    pub desc: String,
    pub cols: Vec<DraftCol>,
    /// table-level UNIQUE column lists, separated by ';'
    pub table_unique: String,
    pub table_check: String,
    pub indexes: Vec<DraftIndex>,
    /// colour in the diagram (one of diagram::TABLE_COLORS)
    pub color: &'static str,
    focus: Field,
    /// field under the mouse, for its hint
    hover: Option<Field>,
    picker: Option<Picker>,
    pub error: Option<String>,
    confirm_discard: bool,
    confirm_submit: bool,
    row_scroll: usize,
    idx_scroll: usize,
    rects: Vec<(Rect, Field)>,
    desc_table: String,
    pub editing: Option<EditInfo>,
    baseline: Snapshot,
}

fn initial_cols() -> Vec<DraftCol> {
    vec![DraftCol { name: "id".into(), ty: "INTEGER".into(), pk: true, ..Default::default() }]
}

/// SQLite accepts literals, CURRENT_* keywords and parenthesised
/// expressions as DEFAULT. Plain words are taken as text.
pub fn default_expr(d: &str) -> String {
    let t = d.trim();
    let up = t.to_uppercase();
    let literal = t.parse::<f64>().is_ok()
        || (t.len() >= 2 && t.starts_with('\'') && t.ends_with('\''))
        || (t.starts_with('(') && t.ends_with(')'))
        || (t.len() >= 3 && (t.starts_with("x'") || t.starts_with("X'")) && t.ends_with('\''))
        || matches!(up.as_str(), "NULL" | "TRUE" | "FALSE" | "CURRENT_TIME" | "CURRENT_DATE" | "CURRENT_TIMESTAMP");
    if literal {
        t.to_string()
    } else if t.contains('(') {
        format!("({t})")
    } else {
        format!("'{}'", t.replace('\'', "''"))
    }
}

/// Uppercase words of a SQL statement, skipping strings and quoted names.
fn sql_words(sql: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in sql.chars() {
        if let Some(q) = quote {
            if ch == q || (q == '[' && ch == ']') {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' | '[' => {
                quote = Some(ch);
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            c if c.is_alphanumeric() || c == '_' => cur.push(c.to_ascii_uppercase()),
            _ => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Parts of a CREATE TABLE that the form has no fields for.
pub fn unsupported_features(sql: &str) -> Vec<String> {
    let w = sql_words(sql);
    let has = |a: &str| w.iter().any(|x| x == a);
    let pair = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    let mut out = Vec::new();
    for (found, what) in [
        // in a CREATE TABLE, AS only occurs in generated columns
        (has("GENERATED") || has("AS"), "generated columns"),
        (pair("WITHOUT", "ROWID"), "WITHOUT ROWID"),
        (has("DEFERRABLE"), "DEFERRABLE"),
        (pair("ON", "CONFLICT"), "ON CONFLICT clauses"),
        (has("MATCH"), "MATCH clauses"),
        // a table option after the closing parenthesis
        (w.last().is_some_and(|x| x == "STRICT") || pair("STRICT", "WITHOUT"), "STRICT"),
    ] {
        if found {
            out.push(what.to_string());
        }
    }
    out
}

/// Split "a, b; c" into column lists.
fn unique_sets(s: &str) -> Vec<String> {
    s.split(';').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// A menu row `width` wide: `left`, then `note` in grey against the right
/// edge (shortened, or left out, when there is no room).
fn with_note<'a>(mut left: Vec<Span<'a>>, note: &str, width: usize, style: Style) -> Line<'a> {
    let used: usize = left.iter().map(|s| s.width()).sum();
    let room = width.saturating_sub(used + 3); // a gap, and a space at the edge
    let note = if note.is_empty() || room < 4 { String::new() } else { truncate(note, room) };
    let n = note.chars().count();
    let gap = width.saturating_sub(used + n + usize::from(n > 0));
    left.push(Span::styled(" ".repeat(gap), style));
    if n > 0 {
        left.push(Span::styled(format!("{note} "), style.fg(Color::DarkGray).remove_modifier(Modifier::BOLD)));
    }
    Line::from(left)
}

impl Draft {
    pub fn new(desc_table: &str) -> Self {
        let mut d = Draft {
            name: String::new(),
            desc: String::new(),
            cols: initial_cols(),
            table_unique: String::new(),
            table_check: String::new(),
            indexes: Vec::new(),
            color: TABLE_COLORS[0].0,
            focus: Field::Name,
            hover: None,
            picker: None,
            error: None,
            confirm_discard: false,
            confirm_submit: false,
            row_scroll: 0,
            idx_scroll: 0,
            rects: Vec::new(),
            desc_table: desc_table.to_string(),
            editing: None,
            baseline: Snapshot::default(),
        };
        d.baseline = d.snapshot();
        d
    }

    /// The form filled in with an existing table.
    pub fn for_table(schema: &Schema, t: usize, desc_table: &str, color: Option<&str>) -> Self {
        let table = &schema.tables[t];
        // UNIQUE written on the column; a separate CREATE UNIQUE INDEX stays an index
        let unique_constraint: HashSet<String> = table
            .indexes
            .iter()
            .filter(|i| i.origin == "u" && i.columns.len() == 1)
            .map(|i| i.columns[0].to_lowercase())
            .collect();
        let cols: Vec<DraftCol> = table
            .columns
            .iter()
            .enumerate()
            .map(|(ci, c)| {
                let fk = schema.fks.iter().find(|f| f.from == ColRef { table: t, column: ci });
                DraftCol {
                    name: c.name.clone(),
                    ty: c.data_type.clone(),
                    pk: c.is_pk(),
                    not_null: c.not_null,
                    unique: !c.is_pk() && unique_constraint.contains(&c.name.to_lowercase()),
                    default: c.default.clone().unwrap_or_default(),
                    refs: fk.map(|f| if f.to.table == t { RefTarget::This(f.to.column) } else { RefTarget::Table(f.to) }),
                    desc: c.description.clone().unwrap_or_default(),
                    orig: Some(ci),
                    collate: c.collate.clone().unwrap_or_default(),
                    check: c.check.clone().unwrap_or_default(),
                    autoinc: c.autoincrement,
                    on_delete: fk.map(|f| f.on_delete).unwrap_or(""),
                    on_update: fk.map(|f| f.on_update).unwrap_or(""),
                }
            })
            .collect();
        let mut lost = unsupported_features(table.sql.as_deref().unwrap_or(""));
        lost.extend(table.unread.iter().cloned());
        if table.composite_fks {
            lost.push("foreign keys over several columns".into());
        }
        let mut d = Draft::new(desc_table);
        d.name = table.name.clone();
        d.desc = table.description.clone().unwrap_or_default();
        d.color = color.and_then(|c| TABLE_COLORS.iter().find(|x| x.0.eq_ignore_ascii_case(c))).map(|x| x.0).unwrap_or(TABLE_COLORS[0].0);
        d.cols = cols;
        // single-column table UNIQUE (a) shows as the column's UQ box instead
        d.table_unique = table.uniques.iter().filter(|u| u.contains(',')).cloned().collect::<Vec<_>>().join("; ");
        d.table_check = match table.checks.len() {
            0 => String::new(),
            1 => table.checks[0].clone(),
            _ => table.checks.iter().map(|c| format!("({c})")).collect::<Vec<_>>().join(" AND "),
        };
        d.indexes = table
            .indexes
            .iter()
            .filter(|i| i.origin == "c")
            .filter_map(|i| {
                let (cols, where_) = parse_index_sql(i.sql.as_deref()?)?;
                Some(DraftIndex { name: i.name.clone(), cols, unique: i.unique, where_ })
            })
            .collect();
        d.baseline = d.snapshot();
        d.editing = Some(EditInfo { table: t, orig_name: table.name.clone(), lost });
        d
    }

    /// A copy of an existing table, as a new table named `name`.
    pub fn duplicate_of(schema: &Schema, t: usize, name: &str, desc_table: &str, color: Option<&str>) -> Self {
        let mut d = Draft::for_table(schema, t, desc_table, color);
        d.editing = None;
        d.name = name.to_string();
        for c in &mut d.cols {
            c.orig = None;
        }
        for ix in &mut d.indexes {
            ix.name.clear(); // index names must be unique: generate new ones
        }
        d
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            name: self.name.clone(),
            desc: self.desc.clone(),
            cols: self.used_cols(),
            table_unique: self.table_unique.trim().to_string(),
            table_check: self.table_check.trim().to_string(),
            indexes: self.indexes.iter().filter(|i| !i.is_blank()).cloned().collect(),
            color: self.color,
        }
    }

    fn used_cols(&self) -> Vec<DraftCol> {
        self.cols.iter().filter(|c| !c.is_blank()).cloned().collect()
    }

    fn is_dirty(&self) -> bool {
        self.snapshot() != self.baseline
    }

    pub fn table_name(&self) -> String {
        self.name.trim().to_string()
    }

    pub fn column_names(&self) -> Vec<String> {
        self.used_cols().iter().map(|c| c.name.trim().to_string()).collect()
    }

    pub fn column_descriptions(&self) -> Vec<(String, String)> {
        self.cols
            .iter()
            .filter(|c| !c.desc.trim().is_empty() && !c.name.trim().is_empty())
            .map(|c| (c.name.trim().to_string(), c.desc.trim().to_string()))
            .collect()
    }

    /// Original column name → new name, for expressions written with the old names.
    fn rename_map(&self, schema: &Schema) -> HashMap<String, String> {
        let Some(ed) = &self.editing else { return HashMap::new() };
        let orig = &schema.tables[ed.table].columns;
        self.cols
            .iter()
            .filter_map(|c| c.orig.map(|o| (orig[o].name.clone(), c.name.trim().to_string())))
            .filter(|(a, b)| a != b && !b.is_empty())
            .map(|(a, b)| (a.to_lowercase(), b))
            .collect()
    }

    // ------------------------------------------------------------ SQL

    pub fn sql(&self, schema: &Schema) -> Result<String, String> {
        self.sql_as(schema, None)
    }

    /// CREATE TABLE for the form; `create_as` names the created table
    /// differently (for a rebuild) while self-references keep the real name.
    pub fn sql_as(&self, schema: &Schema, create_as: Option<&str>) -> Result<String, String> {
        let name = self.table_name();
        let editing = self.editing.as_ref().map(|e| e.table);
        if name.is_empty() {
            return Err("give the table a name".into());
        }
        if name.to_lowercase().starts_with("sqlite_") {
            return Err("names starting with sqlite_ are reserved by SQLite".into());
        }
        if schema.find_table(&name).is_some_and(|t| Some(t) != editing) || crate::schema::is_internal(&name, &self.desc_table) {
            return Err(format!("a table named '{name}' already exists"));
        }
        let used: Vec<&DraftCol> = self.cols.iter().filter(|c| !c.is_blank()).collect();
        if used.is_empty() {
            return Err("add at least one column (arrow down from the last row)".into());
        }
        for (i, c) in self.cols.iter().enumerate() {
            if c.is_blank() {
                continue;
            }
            let n = c.name.trim();
            if n.is_empty() {
                return Err(format!("column {} needs a name", i + 1));
            }
            if self.cols[..i].iter().any(|o| !o.is_blank() && o.name.trim().eq_ignore_ascii_case(n)) {
                return Err(format!("column name '{n}' is used twice"));
            }
        }
        let map = self.rename_map(schema);
        let expr = |s: &str| rename_idents(s.trim(), &map);
        let pks: Vec<&DraftCol> = used.iter().copied().filter(|c| c.pk).collect();
        let single_pk = pks.len() == 1;

        let mut lines = Vec::new();
        for c in &used {
            let mut l = format!("    {}", ident_sql(c.name.trim()));
            if !c.ty.trim().is_empty() {
                l.push(' ');
                l.push_str(c.ty.trim());
            }
            if c.pk && single_pk {
                l.push_str(" PRIMARY KEY");
            }
            if c.autoinc {
                if !(c.pk && single_pk && c.ty.trim().eq_ignore_ascii_case("INTEGER")) {
                    return Err(format!("AUTOINCREMENT on {} needs it to be the only primary key, of type INTEGER", c.name.trim()));
                }
                l.push_str(" AUTOINCREMENT");
            }
            if c.not_null {
                l.push_str(" NOT NULL");
            }
            if c.unique && !c.pk {
                l.push_str(" UNIQUE");
            }
            if !c.default.trim().is_empty() {
                l.push_str(" DEFAULT ");
                l.push_str(&default_expr(&c.default));
            }
            if !c.collate.trim().is_empty() {
                l.push_str(" COLLATE ");
                l.push_str(&ident_sql(c.collate.trim()));
            }
            if !c.check.trim().is_empty() {
                l.push_str(&format!(" CHECK ({})", expr(&c.check)));
            }
            match c.refs {
                Some(RefTarget::Table(r)) => {
                    let t = schema.tables.get(r.table).ok_or("reference to an unknown table")?;
                    let col = t.columns.get(r.column).ok_or("reference to an unknown column")?;
                    l.push_str(&format!(" REFERENCES {}({})", ident_sql(&t.name), ident_sql(&col.name)));
                }
                Some(RefTarget::This(i)) => {
                    let target = self.cols.get(i).filter(|t| t.pk || t.unique).ok_or_else(|| {
                        format!("'{}' references a column of this table that is no longer a key", c.name.trim())
                    })?;
                    l.push_str(&format!(" REFERENCES {}({})", ident_sql(&name), ident_sql(target.name.trim())));
                }
                None => {}
            }
            if c.refs.is_some() {
                if !c.on_delete.is_empty() {
                    l.push_str(&format!(" ON DELETE {}", c.on_delete));
                }
                if !c.on_update.is_empty() {
                    l.push_str(&format!(" ON UPDATE {}", c.on_update));
                }
            }
            lines.push(l);
        }
        if pks.len() > 1 {
            let names: Vec<String> = pks.iter().map(|c| ident_sql(c.name.trim())).collect();
            lines.push(format!("    PRIMARY KEY ({})", names.join(", ")));
        }
        for set in unique_sets(&self.table_unique) {
            lines.push(format!("    UNIQUE ({})", expr(&set)));
        }
        if !self.table_check.trim().is_empty() {
            lines.push(format!("    CHECK ({})", expr(&self.table_check)));
        }
        let created = create_as.map(str::to_string).unwrap_or_else(|| name.clone());
        Ok(format!("CREATE TABLE {} (\n{}\n);", ident_sql(&created), lines.join(",\n")))
    }

    /// CREATE INDEX statements for the form's indexes.
    pub fn index_sqls(&self, schema: &Schema) -> Result<Vec<String>, String> {
        let table = self.table_name();
        let map = self.rename_map(schema);
        let editing = self.editing.as_ref().map(|e| e.table);
        let mut out = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for (i, ix) in self.indexes.iter().enumerate() {
            if ix.is_blank() {
                continue;
            }
            if ix.cols.trim().is_empty() {
                return Err(format!("index {} needs columns, e.g. email or last_name, first_name", i + 1));
            }
            let cols = rename_idents(ix.cols.trim(), &map);
            let name = if ix.name.trim().is_empty() {
                let words: Vec<String> = cols
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|w| !w.is_empty() && !matches!(w.to_uppercase().as_str(), "ASC" | "DESC" | "COLLATE" | "NOCASE" | "RTRIM" | "BINARY"))
                    .map(|w| w.to_lowercase())
                    .collect();
                format!("{}_{}_idx", table.to_lowercase(), words.join("_"))
            } else {
                ix.name.trim().to_string()
            };
            if names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                return Err(format!("two indexes are named {name}"));
            }
            for (ti, t) in schema.tables.iter().enumerate() {
                if Some(ti) != editing && t.indexes.iter().any(|x| x.name.eq_ignore_ascii_case(&name)) {
                    return Err(format!("an index named {name} already exists on {}", t.name));
                }
            }
            names.push(name.clone());
            let mut sql = format!("CREATE {}INDEX {} ON {} ({cols})", if ix.unique { "UNIQUE " } else { "" }, ident_sql(&name), ident_sql(&table));
            if !ix.where_.trim().is_empty() {
                sql.push_str(&format!(" WHERE {}", rename_idents(ix.where_.trim(), &map)));
            }
            sql.push(';');
            out.push(sql);
        }
        Ok(out)
    }

    /// What saving would do to the existing table (edit mode only).
    pub fn change(&self, schema: &Schema) -> Result<Change, String> {
        let Some(ed) = &self.editing else { return Err("not editing a table".into()) };
        self.sql(schema)?; // validates the form
        let index_sqls = self.index_sqls(schema)?;
        let now = self.snapshot();
        let base = &self.baseline;
        if now == *base {
            return Ok(Change::Nothing);
        }
        // everything except names and descriptions
        let shape = |c: &DraftCol| {
            (c.ty.trim().to_string(), c.pk, c.not_null, c.unique, c.default.trim().to_string(), c.refs, c.orig,
             c.collate.trim().to_string(), c.check.trim().to_string(), c.autoinc, c.on_delete, c.on_update)
        };
        let same_shape = now.cols.len() == base.cols.len()
            && now.cols.iter().map(shape).eq(base.cols.iter().map(shape))
            && now.table_unique == base.table_unique
            && now.table_check == base.table_check;
        let same_names = now.name == base.name && now.cols.iter().map(|c| c.name.trim()).eq(base.cols.iter().map(|c| c.name.trim()));
        let same_indexes = now.indexes == base.indexes;
        if same_shape && same_names && same_indexes {
            return Ok(Change::DescriptionsOnly);
        }

        // other tables must keep a valid target
        let t = ed.table;
        let used = &now.cols;
        let pk_count = used.iter().filter(|c| c.pk).count();
        for (ci, oc) in schema.tables[t].columns.iter().enumerate() {
            let from: Vec<String> = schema
                .fks
                .iter()
                .filter(|f| f.to == ColRef { table: t, column: ci } && f.from.table != t)
                .map(|f| schema.qualified(f.from))
                .collect();
            if from.is_empty() {
                continue;
            }
            match used.iter().find(|c| c.orig == Some(ci)) {
                None => {
                    return Err(format!("{} is referenced by {}: remove that reference before removing the column", oc.name, from.join(", ")))
                }
                Some(c) if !((c.pk && pk_count == 1) || c.unique) => {
                    return Err(format!("{} is referenced by {}, so it must stay a single-column primary key or UNIQUE", c.name.trim(), from.join(", ")))
                }
                _ => {}
            }
        }

        let orig_cols = &schema.tables[t].columns;
        let renames = used
            .iter()
            .filter_map(|c| c.orig.map(|o| (orig_cols[o].name.clone(), c.name.trim().to_string())))
            .filter(|(a, b)| a != b)
            .collect();
        let keep: Vec<String> = used.iter().filter(|c| c.orig.is_some()).map(|c| c.name.trim().to_string()).collect();
        let added = used.iter().filter(|c| c.orig.is_none()).map(|c| c.name.trim().to_string()).collect();
        let dropped: Vec<String> =
            (0..orig_cols.len()).filter(|&o| !used.iter().any(|c| c.orig == Some(o))).map(|o| orig_cols[o].name.clone()).collect();
        // renames alone are done in place; anything else needs a rebuild
        let needs_rebuild = !same_shape || !dropped.is_empty();
        let rebuild = if needs_rebuild { Some((self.sql_as(schema, Some(REBUILD_TMP))?, keep)) } else { None };
        let indexes = if same_indexes { None } else { Some(index_sqls) };
        Ok(Change::Apply(Plan { renames, added, dropped, rebuild, indexes }))
    }

    /// Asked before a change that loses something.
    fn confirm_message(&self, schema: &Schema) -> Option<String> {
        let ed = self.editing.as_ref()?;
        let Ok(Change::Apply(plan)) = self.change(schema) else { return None };
        let mut why = Vec::new();
        if !plan.dropped.is_empty() {
            why.push(format!("removes {} and its data", plan.dropped.join(", ")));
        }
        if plan.rebuild.is_some() && !ed.lost.is_empty() {
            why.push(format!("drops {} (not shown in this form)", ed.lost.join(", ")));
        }
        (!why.is_empty()).then(|| format!("this {}. press enter again to apply, any other key to go back", why.join("; ")))
    }

    // ---------------------------------------------------------- editing

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            Field::Name => Some(&mut self.name),
            Field::Desc => Some(&mut self.desc),
            Field::Color => None,
            Field::TableUnique => Some(&mut self.table_unique),
            Field::TableCheck => Some(&mut self.table_check),
            Field::Col(r, f) => {
                let c = self.cols.get_mut(r)?;
                match f {
                    ColField::Name => Some(&mut c.name),
                    ColField::Type => Some(&mut c.ty),
                    ColField::Default => Some(&mut c.default),
                    ColField::Desc => Some(&mut c.desc),
                    ColField::Check => Some(&mut c.check),
                    _ => None,
                }
            }
            Field::Idx(r, f) => {
                let ix = self.indexes.get_mut(r)?;
                match f {
                    IdxField::Name => Some(&mut ix.name),
                    IdxField::Cols => Some(&mut ix.cols),
                    IdxField::Where => Some(&mut ix.where_),
                    IdxField::Unique => None,
                }
            }
        }
    }

    fn is_toggle(&self) -> bool {
        matches!(self.focus, Field::Col(_, f) if f.is_checkbox()) || matches!(self.focus, Field::Idx(_, IdxField::Unique))
    }

    /// A field picked from a fixed list (the choice popup).
    fn is_choice_field(&self) -> bool {
        matches!(self.focus, Field::Col(_, f) if f.is_choice()) || self.focus == Field::Color
    }

    fn toggle(&mut self) {
        match self.focus {
            Field::Col(r, f) => {
                if let Some(c) = self.cols.get_mut(r) {
                    match f {
                        ColField::Pk => c.pk = !c.pk,
                        ColField::NotNull => c.not_null = !c.not_null,
                        ColField::Unique => c.unique = !c.unique,
                        ColField::AutoInc => c.autoinc = !c.autoinc,
                        _ => {}
                    }
                }
            }
            Field::Idx(r, IdxField::Unique) => {
                if let Some(ix) = self.indexes.get_mut(r) {
                    ix.unique = !ix.unique;
                }
            }
            _ => {}
        }
    }

    fn all_fields(&self) -> Vec<Field> {
        let mut v = vec![Field::Name, Field::Desc, Field::Color];
        for r in 0..self.cols.len() {
            v.extend(GRID.iter().map(|&(f, _, _)| Field::Col(r, f)));
        }
        v.push(Field::TableUnique);
        v.push(Field::TableCheck);
        for r in 0..self.indexes.len() {
            v.extend(IDX_FIELDS.iter().map(|&f| Field::Idx(r, f)));
        }
        v
    }

    fn step(&mut self, dir: i64) {
        let all = self.all_fields();
        let i = all.iter().position(|&f| f == self.focus).unwrap_or(0) as i64;
        self.focus = all[(i + dir).rem_euclid(all.len() as i64) as usize];
    }

    /// ↑↓: rows of the form. Past the last column / index a new one is
    /// added; leaving an empty one (up, or down from the last column)
    /// removes it again.
    fn vertical(&mut self, dir: i64) {
        let (n, m) = (self.cols.len(), self.indexes.len());
        let last_col = n.checked_sub(1);
        self.focus = match (self.focus, dir) {
            (Field::Name, 1) => Field::Desc,
            (Field::Desc, -1) => Field::Name,
            (Field::Desc, 1) => Field::Color,
            (Field::Color, -1) => Field::Desc,
            (Field::Color, 1) if n == 0 => {
                self.append_col();
                return;
            }
            (Field::Color, 1) => Field::Col(0, ColField::Name),
            // columns
            (Field::Col(r, _), 1) if Some(r) == last_col && self.cols[r].is_blank() => {
                self.cols.pop();
                Field::TableUnique
            }
            (Field::Col(r, _), 1) if Some(r) == last_col => {
                self.append_col();
                return;
            }
            (Field::Col(r, f), -1) if Some(r) == last_col && self.cols[r].is_blank() => {
                self.cols.pop();
                if r == 0 { Field::Color } else { Field::Col(r - 1, f) }
            }
            (Field::Col(0, _), -1) => Field::Color,
            (Field::Col(r, f), 1) => Field::Col(r + 1, f),
            (Field::Col(r, f), -1) => Field::Col(r - 1, f),
            // table constraints
            (Field::TableUnique, -1) => last_col.map(|r| Field::Col(r, ColField::Name)).unwrap_or(Field::Color),
            (Field::TableUnique, 1) => Field::TableCheck,
            (Field::TableCheck, -1) => Field::TableUnique,
            (Field::TableCheck, 1) if m == 0 => {
                self.append_index();
                return;
            }
            (Field::TableCheck, 1) => Field::Idx(0, IdxField::Name),
            // indexes
            (Field::Idx(r, f), -1) if r + 1 == m && self.indexes[r].is_blank() => {
                self.indexes.pop();
                if r == 0 { Field::TableCheck } else { Field::Idx(r - 1, f) }
            }
            (Field::Idx(0, _), -1) => Field::TableCheck,
            (Field::Idx(r, f), -1) => Field::Idx(r - 1, f),
            (Field::Idx(r, _), 1) if r + 1 == m && !self.indexes[r].is_blank() => {
                self.append_index();
                return;
            }
            (Field::Idx(r, f), 1) if r + 1 < m => Field::Idx(r + 1, f),
            (f, _) => f,
        };
    }

    fn current_row(&self) -> Option<usize> {
        match self.focus {
            Field::Col(r, _) => Some(r),
            _ => None,
        }
    }

    fn append_col(&mut self) {
        self.cols.push(DraftCol { ty: NEW_COL_TYPE.into(), ..Default::default() });
        self.focus = Field::Col(self.cols.len() - 1, ColField::Name);
    }

    fn append_index(&mut self) {
        self.indexes.push(DraftIndex::default());
        self.focus = Field::Idx(self.indexes.len() - 1, IdxField::Cols);
    }

    fn delete_row(&mut self) {
        match self.focus {
            Field::Col(r, f) => {
                self.cols.remove(r);
                for c in &mut self.cols {
                    match c.refs {
                        Some(RefTarget::This(i)) if i == r => c.refs = None,
                        Some(RefTarget::This(i)) if i > r => c.refs = Some(RefTarget::This(i - 1)),
                        _ => {}
                    }
                }
                self.focus = match self.cols.len() {
                    0 => Field::Color,
                    n => Field::Col(r.min(n - 1), f),
                };
            }
            Field::Idx(r, f) => {
                self.indexes.remove(r);
                self.focus = match self.indexes.len() {
                    0 => Field::TableCheck,
                    n => Field::Idx(r.min(n - 1), f),
                };
            }
            _ => {}
        }
    }

    fn move_row(&mut self, dir: i64) {
        let Field::Col(r, f) = self.focus else { return };
        let to = r as i64 + dir;
        if to < 0 || to >= self.cols.len() as i64 {
            return;
        }
        let to = to as usize;
        self.cols.swap(r, to);
        for c in &mut self.cols {
            match c.refs {
                Some(RefTarget::This(i)) if i == r => c.refs = Some(RefTarget::This(to)),
                Some(RefTarget::This(i)) if i == to => c.refs = Some(RefTarget::This(r)),
                _ => {}
            }
        }
        self.focus = Field::Col(to, f);
    }

    pub fn on_paste(&mut self, text: &str) {
        let clean: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        if let Some(t) = self.text_mut() {
            t.push_str(&clean);
        }
    }

    pub fn on_key(&mut self, k: KeyEvent, schema: &Schema) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.picker.is_some() {
            self.on_picker_key(k, schema);
            return Outcome::Continue;
        }
        if k.code != KeyCode::Enter {
            self.confirm_submit = false;
        }
        if k.code == KeyCode::Esc {
            if self.is_dirty() && !self.confirm_discard {
                self.confirm_discard = true;
                self.error = Some(if self.editing.is_some() {
                    "Press ESC again to discard your changes".into()
                } else {
                    "Press ESC again to discard this table".into()
                });
                return Outcome::Continue;
            }
            return Outcome::Cancel;
        }
        self.confirm_discard = false;
        self.error = None;
        let on_refs = matches!(self.focus, Field::Col(_, ColField::Refs));
        let on_type = matches!(self.focus, Field::Col(_, ColField::Type));
        match k.code {
            KeyCode::Enter => {
                if !self.confirm_submit {
                    if let Some(msg) = self.confirm_message(schema) {
                        self.confirm_submit = true;
                        self.error = Some(msg);
                        return Outcome::Continue;
                    }
                }
                return Outcome::Submit;
            }
            KeyCode::Char('d') if ctrl => self.delete_row(),
            KeyCode::Char(' ') if ctrl => self.open_index_cols(),
            KeyCode::Char('u') if ctrl => {
                if let Some(t) = self.text_mut() {
                    t.clear();
                }
            }
            KeyCode::Up if ctrl => self.move_row(-1),
            KeyCode::Down if ctrl => self.move_row(1),
            KeyCode::Tab | KeyCode::Right => self.step(1),
            KeyCode::BackTab | KeyCode::Left => self.step(-1),
            KeyCode::Up => self.vertical(-1),
            KeyCode::Down => self.vertical(1),
            KeyCode::Char(c) if !ctrl => {
                if on_refs {
                    let query = if c == ' ' { String::new() } else { c.to_string() };
                    self.picker = Some(Picker::new(PickerKind::Refs, query));
                } else if on_type && c == ' ' {
                    self.picker = Some(Picker::new(PickerKind::Type, String::new()));
                } else if self.is_choice_field() {
                    self.open_choice(if c == ' ' { String::new() } else { c.to_string() });
                } else if c == ' ' && self.index_cols_wants_list() {
                    self.open_index_cols();
                } else if self.is_toggle() {
                    if c == ' ' || c == 'x' || c == 'X' {
                        self.toggle();
                    }
                } else if let Some(t) = self.text_mut() {
                    t.push(c);
                }
            }
            KeyCode::Backspace | KeyCode::Delete => match self.focus {
                Field::Col(r, ColField::Refs) => self.cols[r].refs = None,
                Field::Col(r, ColField::OnDelete) => self.cols[r].on_delete = "",
                Field::Col(r, ColField::OnUpdate) => self.cols[r].on_update = "",
                Field::Col(r, ColField::Collate) => self.cols[r].collate.clear(),
                _ => {
                    if let Some(t) = self.text_mut() {
                        t.pop();
                    }
                }
            },
            _ => {}
        }
        Outcome::Continue
    }

    /// Click: focus a field; toggles checkboxes, opens the reference picker or a choice list.
    pub fn on_click(&mut self, x: u16, y: u16) {
        if self.picker.is_some() {
            return;
        }
        let Some(&(_, field)) = self.rects.iter().find(|(r, _)| r.contains(Position::new(x, y))) else { return };
        self.focus = field;
        self.error = None;
        if matches!(field, Field::Col(_, ColField::Refs)) {
            self.picker = Some(Picker::new(PickerKind::Refs, String::new()));
        } else if self.is_choice_field() {
            self.open_choice(String::new());
        } else if matches!(field, Field::Idx(_, IdxField::Cols)) {
            self.open_index_cols();
        } else if self.is_toggle() {
            self.toggle();
        }
    }

    /// Space in an index's columns opens the column list when the field is
    /// empty; otherwise it types a space (a, b · name DESC).
    fn index_cols_wants_list(&self) -> bool {
        match self.focus {
            Field::Idx(r, IdxField::Cols) => self.indexes.get(r).is_some_and(|ix| ix.cols.trim().is_empty()),
            _ => false,
        }
    }

    /// Mouse over a field: its hint shows below the grid.
    pub fn on_hover(&mut self, x: u16, y: u16) {
        self.hover = self.rects.iter().find(|(r, _)| r.contains(Position::new(x, y))).map(|&(_, f)| f);
    }

    /// What a field is for, shown under the grid for the focused field
    /// (or the one under the mouse). Keys are marked *like this* and
    /// drawn in bold.
    fn hint(&self, field: Field) -> String {
        let s = match field {
            Field::Name => "The table's name (unique)",
            Field::Desc => "What one row represents (optional, shown in the diagram)",
            Field::Color => "*Space* picks the table's colour in the diagram",
            Field::TableUnique => "Columns that must be unique together · separate sets with ;",
            Field::TableCheck => "A rule every row must pass, e.g. start_date <= end_date",
            Field::Idx(_, IdxField::Name) => "The index's name · empty = automatic",
            Field::Idx(_, IdxField::Cols) => "*Space* picks columns (*Ctrl+Space* when filled) · or type them, e.g. name DESC, lower(email)",
            Field::Idx(_, IdxField::Unique) => "*Space* toggles · a unique index forbids duplicate values",
            Field::Idx(_, IdxField::Where) => "Only index rows matching this, e.g. deleted_at IS NULL",
            Field::Col(r, f) => {
                if f == ColField::Name && r + 1 == self.cols.len() && self.cols[r].is_blank() {
                    return "New column: fill it in, or *↑* removes it".into();
                }
                match f {
                    ColField::Name => "The column's name (unique within the table)",
                    ColField::Type => "*Space* picks a type, or type one",
                    ColField::Pk => "*Space* toggles · primary key: identifies each row (several PK boxes = composite key)",
                    ColField::NotNull => "*Space* toggles · NOT NULL: every row needs a value",
                    ColField::Unique => "*Space* toggles · UNIQUE: no duplicate values",
                    ColField::AutoInc => "*Space* toggles · never reuse ids of deleted rows (INTEGER primary key only)",
                    ColField::Default => "Value for new rows, e.g. a number, text, NULL or CURRENT_TIMESTAMP, or an expression like datetime('now')",
                    ColField::Collate => "*Space* picks how text compares",
                    ColField::Check => "A rule every row must pass, e.g. price >= 0 or status IN ('open', 'done')",
                    ColField::Refs => "*Space* or typing picks the referenced column · *Backspace* clears",
                    ColField::OnDelete => "*Space* picks what happens when the referenced row is deleted",
                    ColField::OnUpdate => "*Space* picks what happens when the referenced key changes",
                    ColField::Desc => "What this column holds (optional, shown in the diagram)",
                }
            }
        };
        s.to_string()
    }

    /// A hint as a line: the *marked* keys as bold keycaps, [Space].
    fn hint_line(hint: &str, style: Style) -> Line<'static> {
        let bold = style.add_modifier(Modifier::BOLD);
        let spans: Vec<Span> = hint
            .split('*')
            .enumerate()
            .filter(|(_, s)| !s.is_empty())
            .map(|(i, s)| if i % 2 == 1 { Span::styled(format!("[{s}]"), bold) } else { Span::styled(s.to_string(), style) })
            .collect();
        Line::from(spans)
    }

    // ---------------------------------------------------------- picker

    /// Columns that can be referenced: single-column primary keys and
    /// UNIQUE columns, of existing tables and of this table.
    fn ref_items(&self, schema: &Schema) -> Vec<RefItem> {
        let mut v = Vec::new();
        let editing = self.editing.as_ref().map(|e| e.table);
        for (ti, t) in schema.tables.iter().enumerate() {
            if Some(ti) == editing {
                continue; // offered below as this table's own columns
            }
            let pk_count = t.columns.iter().filter(|c| c.is_pk()).count();
            for (ci, c) in t.columns.iter().enumerate() {
                if (c.is_pk() && pk_count == 1) || c.unique {
                    v.push(RefItem {
                        label: format!("{}.{}", t.name, c.name),
                        ty: c.data_type.clone(),
                        target: RefTarget::Table(ColRef { table: ti, column: ci }),
                    });
                }
            }
        }
        let own = if self.name.trim().is_empty() { "(this table)".to_string() } else { self.name.trim().to_string() };
        let pk_count = self.cols.iter().filter(|c| c.pk).count();
        for (i, c) in self.cols.iter().enumerate() {
            if ((c.pk && pk_count == 1) || c.unique) && !c.name.trim().is_empty() && Some(i) != self.current_row() {
                v.push(RefItem { label: format!("{own}.{} ↺", c.name.trim()), ty: c.ty.clone(), target: RefTarget::This(i) });
            }
        }
        v
    }

    fn filtered_items(&self, schema: &Schema) -> Vec<RefItem> {
        let q = self.picker.as_ref().map(|p| p.query.to_lowercase()).unwrap_or_default();
        self.ref_items(schema).into_iter().filter(|i| i.label.to_lowercase().contains(&q)).collect()
    }

    /// The type menu: (category, type, param, note) matching the filter.
    fn type_items(&self) -> Vec<(&'static str, &'static str, Param, &'static str)> {
        let q = self.picker.as_ref().map(|p| p.query.to_lowercase()).unwrap_or_default();
        TYPE_GROUPS
            .iter()
            .flat_map(|(g, _, types)| types.iter().map(move |&(t, p, n)| (*g, t, p, n)))
            .filter(|(g, t, _, _)| t.to_lowercase().contains(&q) || g.to_lowercase().contains(&q))
            .collect()
    }

    /// The choice list for the focused field, matching the filter.
    fn choice_items(&self) -> Vec<ChoiceItem> {
        // (value, label, note) in the order of COLLATIONS / FK_ACTIONS
        const COLLATE: [(&str, &str); 4] =
            [("default", "= BINARY"), ("BINARY", "exact bytes: a ≠ A"), ("NOCASE", "ignores ASCII case: a = A"), ("RTRIM", "ignores trailing spaces")];
        let (none, cascade) = if self.focus_is(ColField::OnDelete) {
            ("error while still referenced", "delete this row too")
        } else {
            ("error while still referenced", "copy the new key here")
        };
        let actions: [(&str, &str); 5] = [
            ("none", none),
            ("CASCADE", cascade),
            ("SET NULL", "set this column to NULL"),
            ("SET DEFAULT", "set this column to its default"),
            ("RESTRICT", "like none, but checked at once"),
        ];
        let q = self.picker.as_ref().map(|p| p.query.to_lowercase()).unwrap_or_default();
        let item = |value, (label, note): (&'static str, &'static str)| ChoiceItem { label, value, note, swatch: None };
        let all: Vec<ChoiceItem> = match self.focus {
            Field::Col(_, ColField::Collate) => COLLATIONS.iter().zip(COLLATE).map(|(&v, ln)| item(v, ln)).collect(),
            Field::Col(_, ColField::OnDelete | ColField::OnUpdate) => FK_ACTIONS.iter().zip(actions).map(|(&v, ln)| item(v, ln)).collect(),
            Field::Color => TABLE_COLORS
                .iter()
                .enumerate()
                .map(|(i, &(n, c))| ChoiceItem { label: n, value: n, note: if i == 0 { "default" } else { "" }, swatch: Some(c) })
                .collect(),
            _ => Vec::new(),
        };
        let mut v: Vec<_> = all.into_iter().filter(|i| i.label.to_lowercase().contains(&q)).collect();
        // what starts with the filter first: "n" picks NOCASE, not BINARY
        v.sort_by_key(|i| !i.label.to_lowercase().starts_with(&q));
        v
    }

    fn focus_is(&self, f: ColField) -> bool {
        matches!(self.focus, Field::Col(_, g) if g == f)
    }

    /// The focused choice field's current value.
    fn choice_value(&self) -> String {
        match self.focus {
            Field::Col(r, f) => match (self.cols.get(r), f) {
                (Some(c), ColField::Collate) => c.collate.trim().to_uppercase(),
                (Some(c), ColField::OnDelete) => c.on_delete.to_string(),
                (Some(c), ColField::OnUpdate) => c.on_update.to_string(),
                _ => String::new(),
            },
            Field::Color => self.color.to_string(),
            _ => String::new(),
        }
    }

    /// Open the choice list, with the current value highlighted (or the
    /// best match when a letter opened it).
    fn open_choice(&mut self, query: String) {
        let typed = !query.is_empty();
        self.picker = Some(Picker::new(PickerKind::Choice, query));
        let current = self.choice_value();
        let idx = if typed { 0 } else { self.choice_items().iter().position(|i| i.value == current).unwrap_or(0) };
        if let Some(p) = &mut self.picker {
            p.idx = idx;
        }
    }

    /// Write a value from the choice list to the focused field.
    fn set_choice(&mut self, value: &'static str) {
        match self.focus {
            Field::Col(r, f) => {
                if let Some(c) = self.cols.get_mut(r) {
                    match f {
                        ColField::Collate => c.collate = value.to_string(),
                        ColField::OnDelete => c.on_delete = value,
                        ColField::OnUpdate => c.on_update = value,
                        _ => {}
                    }
                }
            }
            Field::Color => self.color = if value.is_empty() { TABLE_COLORS[0].0 } else { value },
            _ => {}
        }
    }

    fn on_choice_picker_key(&mut self, k: KeyEvent) {
        let items = self.choice_items();
        let Some(p) = &mut self.picker else { return };
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.idx = p.idx.saturating_sub(1),
            KeyCode::Down => p.idx = (p.idx + 1).min(items.len().saturating_sub(1)),
            KeyCode::Backspace => {
                p.query.pop();
                p.idx = 0;
            }
            KeyCode::Delete => {
                self.set_choice("");
                self.picker = None;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                p.query.push(c);
                p.idx = 0;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if let Some(item) = items.get(p.idx) {
                    self.set_choice(item.value);
                }
                self.picker = None;
            }
            _ => {}
        }
    }

    /// The table's columns for the index popup, (name, type), matching the filter.
    fn index_col_items(&self) -> Vec<(String, String)> {
        let q = self.picker.as_ref().map(|p| p.query.to_lowercase()).unwrap_or_default();
        self.cols
            .iter()
            .filter(|c| !c.name.trim().is_empty() && c.name.to_lowercase().contains(&q))
            .map(|c| (c.name.trim().to_string(), c.ty.trim().to_string()))
            .collect()
    }

    /// Open the index column popup on the focused index, with the columns
    /// already in it ticked.
    fn open_index_cols(&mut self) {
        let Field::Idx(r, IdxField::Cols) = self.focus else { return };
        let mut p = Picker::new(PickerKind::IndexCols, String::new());
        p.parts = self.indexes.get(r).map(|ix| split_top_level(&ix.cols)).unwrap_or_default();
        self.picker = Some(p);
    }

    fn on_index_cols_key(&mut self, k: KeyEvent) {
        let items = self.index_col_items();
        let Field::Idx(r, IdxField::Cols) = self.focus else { return };
        let Some(p) = &mut self.picker else { return };
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.idx = p.idx.saturating_sub(1),
            KeyCode::Down => p.idx = (p.idx + 1).min(items.len().saturating_sub(1)),
            KeyCode::Backspace => {
                p.query.pop();
                p.idx = 0;
            }
            KeyCode::Char(' ') => {
                if let Some((name, _)) = items.get(p.idx) {
                    let at = p.parts.iter().position(|x| part_column(x).is_some_and(|c| c.eq_ignore_ascii_case(name)));
                    match at {
                        Some(i) => {
                            p.parts.remove(i);
                        }
                        None => p.parts.push(ident_sql(name)),
                    }
                }
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                p.query.push(c);
                p.idx = 0;
            }
            KeyCode::Delete => {
                self.indexes[r].cols.clear();
                self.picker = None;
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.indexes[r].cols = p.parts.join(", ");
                self.picker = None;
            }
            _ => {}
        }
    }

    fn on_type_picker_key(&mut self, k: KeyEvent) {
        let items = self.type_items();
        let row = self.current_row();
        let Some(p) = &mut self.picker else { return };
        let current = row.map(|r| self.cols[r].ty.clone()).unwrap_or_default();
        // second step: the number for NVARCHAR(…), DECIMAL(…)
        if let Some((ty, param, text)) = &mut p.param {
            match k.code {
                KeyCode::Esc => p.param = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if c.is_ascii_digit() || (c == ',' && *param == Param::Precision && !text.contains(',')) => text.push(c),
                KeyCode::Enter => {
                    let ok = match param {
                        Param::Length => text.parse::<u32>().is_ok_and(|n| n > 0),
                        _ => {
                            let parts: Vec<&str> = text.split(',').map(str::trim).collect();
                            parts.iter().all(|x| x.parse::<u32>().is_ok()) && !parts.is_empty() && parts.len() <= 2
                        }
                    };
                    if ok {
                        if let Some(r) = row {
                            self.cols[r].ty = format!("{ty}({})", text.replace(' ', ""));
                        }
                        self.picker = None;
                    }
                }
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.idx = p.idx.saturating_sub(1),
            KeyCode::Down => p.idx = (p.idx + 1).min(items.len().saturating_sub(1)),
            KeyCode::Backspace => {
                p.query.pop();
                p.idx = 0;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) && c != ' ' => {
                p.query.push(c);
                p.idx = 0;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if let Some(&(_, ty, param, _)) = items.get(p.idx) {
                    if param == Param::No {
                        if let Some(r) = row {
                            self.cols[r].ty = ty.to_string();
                        }
                        self.picker = None;
                    } else {
                        // keep the number already there for the same type
                        let prev = current
                            .strip_prefix(ty)
                            .and_then(|rest| rest.trim().strip_prefix('('))
                            .and_then(|rest| rest.strip_suffix(')'))
                            .map(str::to_string);
                        let default = if param == Param::Length { "255" } else { "10,2" };
                        p.param = Some((ty, param, prev.unwrap_or_else(|| default.to_string())));
                    }
                }
            }
            _ => {}
        }
    }

    fn on_picker_key(&mut self, k: KeyEvent, schema: &Schema) {
        match self.picker.as_ref().map(|p| p.kind) {
            Some(PickerKind::Type) => return self.on_type_picker_key(k),
            Some(PickerKind::Choice) => return self.on_choice_picker_key(k),
            Some(PickerKind::IndexCols) => return self.on_index_cols_key(k),
            _ => {}
        }
        let items = self.filtered_items(schema);
        let row = self.current_row();
        let Some(p) = &mut self.picker else { return };
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.idx = p.idx.saturating_sub(1),
            KeyCode::Down => p.idx = (p.idx + 1).min(items.len().saturating_sub(1)),
            KeyCode::Backspace => {
                p.query.pop();
                p.idx = 0;
            }
            KeyCode::Delete => {
                if let Some(r) = row {
                    self.cols[r].refs = None;
                }
                self.picker = None;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                p.query.push(c);
                p.idx = 0;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if let (Some(item), Some(r)) = (items.get(p.idx), row) {
                    let col = &mut self.cols[r];
                    col.refs = Some(item.target);
                    if col.ty.trim().is_empty() || col.ty == NEW_COL_TYPE {
                        col.ty = item.ty.clone(); // a FK has the type of what it points at
                    }
                }
                self.picker = None;
            }
            _ => {}
        }
    }

    fn ref_label(&self, r: Option<RefTarget>, schema: &Schema) -> String {
        match r {
            None => String::new(),
            Some(RefTarget::Table(c)) => schema.qualified(c),
            Some(RefTarget::This(i)) => format!("↺ {}", self.cols.get(i).map(|c| c.name.trim()).unwrap_or("?")),
        }
    }

    // ---------------------------------------------------------- drawing

    /// The lines under the SQL box: an error, and what saving would do.
    fn save_status(&self, schema: &Schema) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let warn = Style::default().fg(Color::Yellow);
        let gray = Style::default().fg(Color::Gray);
        if let Some(e) = &self.error {
            let color = if self.confirm_submit { Color::Yellow } else { Color::Red };
            out.push(Line::styled(format!("✗ {e}"), Style::default().fg(color).add_modifier(Modifier::BOLD)));
        }
        if self.editing.is_none() {
            return out;
        }
        match self.change(schema) {
            Ok(Change::Nothing) => out.push(Line::styled("no changes yet", Style::default().fg(Color::DarkGray))),
            Ok(Change::DescriptionsOnly) => {
                out.push(Line::styled("only descriptions changed: the table itself is left untouched", Style::default().fg(Color::Green)))
            }
            Ok(Change::Apply(plan)) => {
                let mut notes = Vec::new();
                if self.table_name() != self.baseline.name {
                    notes.push(format!("renames the table to {}", self.table_name()));
                }
                notes.extend(plan.renames.iter().map(|(a, b)| format!("{a} → {b}")));
                if !plan.added.is_empty() {
                    notes.push(format!("adds {}", plan.added.join(", ")));
                }
                if plan.indexes.is_some() {
                    notes.push("replaces the indexes".into());
                }
                if !notes.is_empty() {
                    out.push(Line::styled(notes.join(" · "), gray));
                }
                if !plan.dropped.is_empty() {
                    out.push(Line::styled(format!("removes {} (their data is lost)", plan.dropped.join(", ")), warn));
                }
                if let (Some(ed), true) = (&self.editing, plan.rebuild.is_some()) {
                    if !ed.lost.is_empty() {
                        out.push(Line::styled(
                            format!("⚠ the current definition has {}: this form can't show it, so saving removes it", ed.lost.join(", ")),
                            warn,
                        ));
                    }
                }
                // last: the bottom line of the form
                out.push(if plan.rebuild.is_some() {
                    Line::styled(
                        "saving rebuilds the table and copies its rows over (SQLite can't change a table in place)",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Line::styled("saving changes this in place, without rebuilding the table", gray)
                });
            }
            Err(_) => {}
        }
        out
    }

    fn draw_field(&mut self, f: &mut Frame, rect: Rect, field: Field, text: &str, placeholder: &str) {
        if rect.width == 0 {
            return;
        }
        let focused = self.focus == field;
        let w = rect.width as usize;
        let shown = if text.chars().count() >= w && w > 1 {
            // show the end of long text, where you're typing
            let tail: String = text.chars().skip(text.chars().count() + 2 - w).collect();
            format!("…{tail}")
        } else {
            text.to_string()
        };
        let (content, style) = match (text.is_empty(), focused) {
            (true, true) => (String::new(), Style::default().fg(Color::Black).bg(HL_COLOR)),
            (true, false) => (truncate(placeholder, w), Style::default().fg(Color::DarkGray).bg(Color::Black)),
            (false, true) => (shown, Style::default().fg(Color::Black).bg(HL_COLOR)),
            (false, false) => (shown, Style::default().fg(Color::White).bg(Color::Black)),
        };
        f.render_widget(Span::styled(format!("{content:<w$}"), style), rect);
        let is_text = !matches!(field, Field::Col(_, ColField::Refs | ColField::OnDelete | ColField::OnUpdate | ColField::Collate) | Field::Color);
        if focused && self.picker.is_none() && is_text {
            let cx = rect.x + (content.chars().count() as u16).min(rect.width.saturating_sub(1));
            f.set_cursor_position(Position::new(cx, rect.y));
        }
        self.rects.push((rect, field));
    }

    fn draw_checkbox(&mut self, f: &mut Frame, rect: Rect, field: Field, on: bool, color: Color) {
        if rect.width == 0 {
            return;
        }
        let style = if self.focus == field {
            Style::default().fg(Color::Black).bg(HL_COLOR).add_modifier(Modifier::BOLD)
        } else if on {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        f.render_widget(Span::styled(if on { "[x]" } else { "[ ]" }, style), rect);
        self.rects.push((rect, field));
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, schema: &Schema) {
        self.rects.clear();
        // everything below the app's header bar
        let top = area.height.min(1);
        let rect = Rect::new(area.x, area.y + top, area.width, area.height - top);
        let (title, enter) = match &self.editing {
            Some(e) => (format!(" Edit table {} ", e.orig_name), "enter save"),
            None => (" New table ".to_string(), "enter create"),
        };
        let block = Block::bordered()
            .border_style(Style::default().fg(HL_COLOR))
            .title(title)
            .title_bottom(Line::styled(
                format!(" tab/←→ field · ↑↓ row (↓ past the last adds one) · space toggle/pick · ctrl+d delete row · ctrl+↑↓ reorder · {enter} · esc cancel "),
                Style::default().fg(Color::DarkGray),
            ));
        let inner = block.inner(rect);
        f.render_widget(Clear, rect);
        f.render_widget(block, rect);
        if inner.width < 30 || inner.height < 12 {
            f.render_widget(Paragraph::new("terminal too small for the table form"), inner);
            return;
        }
        let label = Style::default().fg(Color::Gray);
        let dim = Style::default().fg(Color::DarkGray);
        let head = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let (x0, w) = (inner.x, inner.width);
        let right = inner.right();
        // a rect that stays inside the form
        let cell = |x: u16, y: u16, width: u16| Rect::new(x.min(right), y, width.min(right.saturating_sub(x)), 1);
        let mut y = inner.y;

        // table name and description
        let lw = 13u16;
        f.render_widget(Span::styled("Table name", label), cell(x0, y, lw));
        let name = self.name.clone();
        self.draw_field(f, cell(x0 + lw, y, (w - lw).min(40)), Field::Name, &name, "e.g. orders");
        y += 1;
        f.render_widget(Span::styled("Description", label), cell(x0, y, lw));
        let desc = self.desc.clone();
        self.draw_field(f, cell(x0 + lw, y, w - lw), Field::Desc, &desc, "what one row of this table represents (optional)");
        y += 1;
        f.render_widget(Span::styled("Colour", label), cell(x0, y, lw));
        let swatch = TABLE_COLORS.iter().find(|c| c.0 == self.color).map(|c| c.1).unwrap_or(Color::White);
        f.render_widget(Span::styled("██", Style::default().fg(swatch)), cell(x0 + lw, y, 2));
        let color = self.color;
        self.draw_field(f, cell(x0 + lw + 3, y, 10), Field::Color, color, "");
        y += 2;

        // what saving would do (or what's wrong), at the bottom of the form
        let status = self.save_status(schema);

        // space: the SQL box gets what's left after the fixed parts
        let idx_rows = self.indexes.len().clamp(1, 5);
        // header, gap, constraints, gap, idx header, rows, gap, hint, gap, SQL box min, status
        let fixed = 1 + 1 + 1 + 1 + 1 + idx_rows + 1 + 1 + 1 + 3 + status.len();
        let room = (inner.bottom().saturating_sub(y) as usize).saturating_sub(fixed);
        let rows_h = self.cols.len().max(1).min(room.max(1));

        // grid geometry: base widths, squeezed on narrow terminals; the
        // description takes what's left. A │ separates every column.
        let seps = GRID.len() as u16 - 1;
        let avail = w.saturating_sub(3 + seps);
        let base_total: u16 = GRID.iter().map(|g| g.2).sum();
        let widths: Vec<u16> = if avail >= base_total + 12 {
            GRID.iter().map(|g| if g.2 == 0 { avail - base_total } else { g.2 }).collect()
        } else {
            let scale = |b: u16| ((b as u32 * avail as u32) / (base_total as u32 + 12)).max(3) as u16;
            let mut v: Vec<u16> = GRID.iter().map(|g| if g.2 == 3 { 3 } else if g.2 == 0 { 0 } else { scale(g.2) }).collect();
            let used: u16 = v.iter().sum();
            v[GRID.len() - 1] = avail.saturating_sub(used).max(3);
            v
        };
        let xs: Vec<u16> = {
            let mut x = x0 + 3;
            widths.iter().map(|&cw| { let at = x; x += cw + 1; at }).collect()
        };
        let sep_style = Style::default().fg(Color::DarkGray);
        let draw_seps = |f: &mut Frame, y: u16| {
            for i in 0..GRID.len() - 1 {
                f.render_widget(Span::styled("│", sep_style), cell(xs[i] + widths[i], y, 1));
            }
        };
        for (i, (_, h, _)) in GRID.iter().enumerate() {
            f.render_widget(Span::styled(truncate(h, widths[i] as usize), head), cell(xs[i], y, widths[i]));
        }
        draw_seps(f, y);
        y += 1;

        if let Some(r) = self.current_row() {
            if r < self.row_scroll {
                self.row_scroll = r;
            } else if r >= self.row_scroll + rows_h {
                self.row_scroll = r + 1 - rows_h;
            }
        }
        self.row_scroll = self.row_scroll.min(self.cols.len().saturating_sub(rows_h));
        if self.cols.is_empty() {
            f.render_widget(Span::styled("  no columns: arrow down to add one", dim), cell(x0, y, w));
        }
        let first_row_y = y;
        for r in self.row_scroll..(self.row_scroll + rows_h).min(self.cols.len()) {
            let c = self.cols[r].clone();
            let num_color = if c.pk { PK_COLOR } else if c.refs.is_some() { FK_COLOR } else { Color::DarkGray };
            f.render_widget(Span::styled(format!("{:>2}", r + 1), Style::default().fg(num_color)), cell(x0, y, 2));
            let refs = self.ref_label(c.refs, schema);
            let no_ref = if c.refs.is_some() { "none" } else { "—" };
            for (i, &(field, _, _)) in GRID.iter().enumerate() {
                let rect = cell(xs[i], y, widths[i]);
                let fid = Field::Col(r, field);
                let focused = self.focus == fid;
                match field {
                    ColField::Pk => self.draw_checkbox(f, rect, fid, c.pk, PK_COLOR),
                    ColField::NotNull => self.draw_checkbox(f, rect, fid, c.not_null, Color::White),
                    ColField::Unique => self.draw_checkbox(f, rect, fid, c.unique, Color::White),
                    ColField::AutoInc => self.draw_checkbox(f, rect, fid, c.autoinc, PK_COLOR),
                    ColField::Name => self.draw_field(f, rect, fid, &c.name, "name"),
                    ColField::Type => self.draw_field(f, rect, fid, &c.ty, if focused { "space: list" } else { "type" }),
                    ColField::Default => self.draw_field(f, rect, fid, &c.default, ""),
                    ColField::Collate => self.draw_field(f, rect, fid, &c.collate, "—"),
                    ColField::Check => self.draw_field(f, rect, fid, &c.check, ""),
                    ColField::Refs => self.draw_field(f, rect, fid, &refs, if focused { "space: pick" } else { "—" }),
                    ColField::OnDelete => self.draw_field(f, rect, fid, c.on_delete, no_ref),
                    ColField::OnUpdate => self.draw_field(f, rect, fid, c.on_update, no_ref),
                    ColField::Desc => self.draw_field(f, rect, fid, &c.desc, ""),
                }
            }
            draw_seps(f, y);
            y += 1;
        }
        y = first_row_y + rows_h as u16 + 1;

        // table constraints
        f.render_widget(Span::styled("UNIQUE", label), cell(x0, y, 7));
        let uw = (w.saturating_sub(15)) / 2;
        let tu = self.table_unique.clone();
        self.draw_field(f, cell(x0 + 7, y, uw), Field::TableUnique, &tu, "several columns together: a, b ; c, d");
        f.render_widget(Span::styled(" CHECK", label), cell(x0 + 7 + uw, y, 7));
        let tc = self.table_check.clone();
        self.draw_field(f, cell(x0 + 14 + uw, y, right.saturating_sub(x0 + 14 + uw)), Field::TableCheck, &tc, "table check, e.g. start_date <= end_date");
        y += 2;

        // indexes
        let iw = [20u16, 30, 3];
        let ixs = [x0 + 3, x0 + 24, x0 + 55, x0 + 59];
        for (i, h) in ["Index", "Columns", "UQ", "Where (partial index)"].iter().enumerate() {
            let width = if i < 3 { iw[i] } else { right.saturating_sub(ixs[3]) };
            f.render_widget(Span::styled(h.to_string(), head), cell(ixs[i], y, width));
        }
        y += 1;
        if let Field::Idx(r, _) = self.focus {
            if r < self.idx_scroll {
                self.idx_scroll = r;
            } else if r >= self.idx_scroll + idx_rows {
                self.idx_scroll = r + 1 - idx_rows;
            }
        }
        self.idx_scroll = self.idx_scroll.min(self.indexes.len().saturating_sub(idx_rows));
        if self.indexes.is_empty() {
            f.render_widget(Span::styled("   no indexes · arrow down from CHECK to add one", dim), cell(x0, y, w));
        }
        let first_idx_y = y;
        for r in self.idx_scroll..(self.idx_scroll + idx_rows).min(self.indexes.len()) {
            let ix = self.indexes[r].clone();
            let auto = if self.focus == Field::Idx(r, IdxField::Name) { "" } else { "(automatic name)" };
            self.draw_field(f, cell(ixs[0], y, iw[0]), Field::Idx(r, IdxField::Name), &ix.name, auto);
            let hint = if self.focus == Field::Idx(r, IdxField::Cols) { "space: pick columns" } else { "e.g. last_name, first_name" };
            self.draw_field(f, cell(ixs[1], y, iw[1]), Field::Idx(r, IdxField::Cols), &ix.cols, hint);
            self.draw_checkbox(f, cell(ixs[2], y, 3), Field::Idx(r, IdxField::Unique), ix.unique, Color::White);
            self.draw_field(f, cell(ixs[3], y, right.saturating_sub(ixs[3])), Field::Idx(r, IdxField::Where), &ix.where_, "");
            y += 1;
        }
        y = first_idx_y + idx_rows as u16 + 1;

        // what the focused field (or the one under the mouse) is for
        let hint = self.hint(self.hover.unwrap_or(self.focus));
        if y < inner.bottom() {
            f.render_widget(Self::hint_line(&hint, dim.add_modifier(Modifier::ITALIC)), cell(x0, y, w));
            y += 2; // and a blank line before the SQL box
        }

        // the status against the bottom edge, the SQL box in between
        let status_y = inner.bottom().saturating_sub(status.len() as u16).max(y);
        if status_y < inner.bottom() {
            f.render_widget(Paragraph::new(status), Rect::new(x0, status_y, w, inner.bottom() - status_y));
        }
        let mut sql_lines: Vec<Line> = Vec::new();
        let quiet = self.error.is_none() || self.confirm_submit;
        match (self.sql(schema), self.index_sqls(schema)) {
            (Ok(sql), Ok(ix)) if quiet => {
                let gray = Style::default().fg(Color::Gray);
                sql_lines.extend(sql.lines().map(|l| Line::styled(l.to_string(), gray)));
                sql_lines.extend(ix.into_iter().map(|l| Line::styled(l, gray)));
            }
            (Err(e), _) | (_, Err(e)) if self.error.is_none() => sql_lines.push(Line::styled(format!("… {e}"), Style::default().fg(Color::Yellow))),
            _ => {}
        }
        if status_y >= y + 3 {
            let block = Block::bordered().border_style(dim).title(Span::styled(" SQL ", dim));
            let r = Rect::new(x0, y, w, status_y - y);
            f.render_widget(Paragraph::new(sql_lines).block(block), r);
        }

        // type menu on top
        if self.picker.as_ref().is_some_and(|p| p.kind == PickerKind::Type) {
            let items = self.type_items();
            let p = self.picker.as_mut().unwrap();
            p.idx = p.idx.min(items.len().saturating_sub(1));
            let (idx, query, param) = (p.idx, p.query.clone(), p.param.clone());
            // category headers take a line each
            let mut lines: Vec<(Option<usize>, Line)> = Vec::new();
            let mut last_group = "";
            let pw = 52.min(area.width);
            let lw = pw.saturating_sub(2) as usize;
            for (i, (g, t, pm, note)) in items.iter().enumerate() {
                if *g != last_group {
                    let group_note = TYPE_GROUPS.iter().find(|x| x.0 == *g).map(|x| x.1).unwrap_or("");
                    let head = Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD);
                    lines.push((None, with_note(vec![Span::styled(format!(" {g}"), head)], group_note, lw, Style::default())));
                    last_group = g;
                }
                let arg = match pm {
                    Param::No => "",
                    Param::Length => "(n)",
                    Param::Precision => "(p,s)",
                };
                let style = if i == idx { Style::default().fg(Color::Black).bg(FK_COLOR) } else { Style::default() };
                lines.push((Some(i), with_note(vec![Span::styled(format!("   {t}{arg}"), style)], note, lw, style)));
            }
            let ph = (lines.len() as u16 + 3).clamp(6, 24).min(area.height);
            let pr = Rect::new(area.x + (area.width - pw) / 2, area.y + (area.height - ph) / 2, pw, ph);
            let bottom = if param.is_some() { " digits · enter ok · esc back " } else { " type to filter · ↑↓ · enter pick · esc " };
            let block = Block::bordered().border_style(Style::default().fg(FK_COLOR)).title(" type… ").title_bottom(Line::styled(bottom, dim));
            let pin = block.inner(pr);
            f.render_widget(Clear, pr);
            f.render_widget(block, pr);
            let top_line = match &param {
                Some((ty, pm, text)) => {
                    let what = if *pm == Param::Length { "length" } else { "precision, scale" };
                    Line::from(vec![
                        Span::styled(format!("{ty} {what}: "), Style::default().fg(HL_COLOR).add_modifier(Modifier::BOLD)),
                        Span::raw(text.clone()),
                    ])
                }
                None => Line::from(vec![Span::styled("filter: ", dim), Span::raw(query.clone())]),
            };
            let cursor_x = pin.x + top_line.width() as u16;
            f.render_widget(top_line, Rect::new(pin.x, pin.y, pin.width, 1));
            f.set_cursor_position(Position::new(cursor_x.min(pin.right().saturating_sub(1)), pin.y));
            let visible = pin.height.saturating_sub(1) as usize;
            let sel_line = lines.iter().position(|(i, _)| *i == Some(idx)).unwrap_or(0);
            let first = sel_line.saturating_sub(visible.saturating_sub(1));
            for (n, (_, line)) in lines.into_iter().skip(first).take(visible).enumerate() {
                f.render_widget(line, Rect::new(pin.x, pin.y + 1 + n as u16, pin.width, 1));
            }
            return;
        }

        // index columns on top
        if self.picker.as_ref().is_some_and(|p| p.kind == PickerKind::IndexCols) {
            let items = self.index_col_items();
            let p = self.picker.as_mut().unwrap();
            p.idx = p.idx.min(items.len().saturating_sub(1));
            let (idx, query, parts) = (p.idx, p.query.clone(), p.parts.clone());
            let pw = 60.min(area.width);
            let ph = (items.len() as u16 + 4).clamp(6, 18).min(area.height);
            let pr = Rect::new(area.x + (area.width - pw) / 2, area.y + (area.height - ph) / 2, pw, ph);
            let block = Block::bordered()
                .border_style(Style::default().fg(FK_COLOR))
                .title(" index columns… ")
                .title_bottom(Line::styled(" space tick · type to filter · ↑↓ · enter ok · del clear · esc ", dim));
            let pin = block.inner(pr);
            f.render_widget(Clear, pr);
            f.render_widget(block, pr);
            f.render_widget(
                Line::from(vec![Span::styled("filter: ", dim), Span::raw(query.clone())]),
                Rect::new(pin.x, pin.y, pin.width, 1),
            );
            // what the field will hold, in order
            let preview = if parts.is_empty() {
                Span::styled("tick columns in the order the index should use", dim)
            } else {
                Span::styled(parts.join(", "), Style::default().fg(HL_COLOR))
            };
            f.render_widget(Line::from(vec![Span::styled("index:  ", dim), preview]), Rect::new(pin.x, pin.y + 1, pin.width, 1));
            f.set_cursor_position(Position::new((pin.x + 8 + query.chars().count() as u16).min(pin.right().saturating_sub(1)), pin.y));
            let visible = pin.height.saturating_sub(2) as usize;
            let first = idx.saturating_sub(visible.saturating_sub(1));
            if items.is_empty() {
                f.render_widget(Span::styled("no matching columns", dim), Rect::new(pin.x, pin.y + 2, pin.width, 1));
            }
            for (i, (name, ty)) in items.iter().enumerate().skip(first).take(visible) {
                let style = if i == idx { Style::default().fg(Color::Black).bg(FK_COLOR) } else { Style::default() };
                let at = parts.iter().position(|x| part_column(x).is_some_and(|c| c.eq_ignore_ascii_case(name)));
                let text = match at {
                    Some(n) => format!(" [x] {} {}", n + 1, parts[n]),
                    None => format!(" [ ]   {name}"),
                };
                let line = with_note(vec![Span::styled(text, style)], ty, pin.width as usize, style);
                f.render_widget(line, Rect::new(pin.x, pin.y + 2 + (i - first) as u16, pin.width, 1));
            }
            return;
        }

        // choice list on top
        if self.picker.as_ref().is_some_and(|p| p.kind == PickerKind::Choice) {
            let items = self.choice_items();
            let p = self.picker.as_mut().unwrap();
            p.idx = p.idx.min(items.len().saturating_sub(1));
            let (idx, query) = (p.idx, p.query.clone());
            let title = match self.focus {
                Field::Col(_, ColField::Collate) => " collate… ",
                Field::Col(_, ColField::OnDelete) => " on delete… ",
                Field::Col(_, ColField::OnUpdate) => " on update… ",
                _ => " colour… ",
            };
            let pw = 56.min(area.width);
            let ph = (items.len() as u16 + 3).clamp(5, 12).min(area.height);
            let pr = Rect::new(area.x + (area.width - pw) / 2, area.y + (area.height - ph) / 2, pw, ph);
            let block = Block::bordered()
                .border_style(Style::default().fg(FK_COLOR))
                .title(title)
                .title_bottom(Line::styled(" type to filter · ↑↓ · enter pick · del clear · esc ", dim));
            let pin = block.inner(pr);
            f.render_widget(Clear, pr);
            f.render_widget(block, pr);
            f.render_widget(
                Line::from(vec![Span::styled("filter: ", dim), Span::raw(query.clone())]),
                Rect::new(pin.x, pin.y, pin.width, 1),
            );
            f.set_cursor_position(Position::new((pin.x + 8 + query.chars().count() as u16).min(pin.right().saturating_sub(1)), pin.y));
            let visible = pin.height.saturating_sub(1) as usize;
            let first = idx.saturating_sub(visible.saturating_sub(1));
            if items.is_empty() {
                f.render_widget(Span::styled("nothing matches", dim), Rect::new(pin.x, pin.y + 1, pin.width, 1));
            }
            for (i, item) in items.iter().enumerate().skip(first).take(visible) {
                let style = if i == idx { Style::default().fg(Color::Black).bg(FK_COLOR) } else { Style::default() };
                let mut spans = vec![Span::styled(" ", style)];
                if let Some(c) = item.swatch {
                    spans.push(Span::styled("██", style.fg(c)));
                    spans.push(Span::styled(" ", style));
                }
                spans.push(Span::styled(item.label, style));
                let line = with_note(spans, item.note, pin.width as usize, style);
                f.render_widget(line, Rect::new(pin.x, pin.y + 1 + (i - first) as u16, pin.width, 1));
            }
            return;
        }

        // reference picker on top
        if self.picker.as_ref().is_some_and(|p| p.kind == PickerKind::Refs) {
            let items = self.filtered_items(schema);
            let p = self.picker.as_mut().unwrap();
            p.idx = p.idx.min(items.len().saturating_sub(1));
            let (idx, query) = (p.idx, p.query.clone());
            let pw = 56.min(area.width);
            let ph = (items.len() as u16 + 3).clamp(5, 18).min(area.height);
            let pr = Rect::new(area.x + (area.width - pw) / 2, area.y + (area.height - ph) / 2, pw, ph);
            let block = Block::bordered().border_style(Style::default().fg(FK_COLOR)).title(" references… ").title_bottom(
                Line::styled(" type to filter · ↑↓ · enter pick · del none · esc ", dim),
            );
            let pin = block.inner(pr);
            f.render_widget(Clear, pr);
            f.render_widget(block, pr);
            f.render_widget(
                Line::from(vec![Span::styled("filter: ", dim), Span::raw(query.clone())]),
                Rect::new(pin.x, pin.y, pin.width, 1),
            );
            f.set_cursor_position(Position::new(pin.x + 8 + query.chars().count() as u16, pin.y));
            let visible = pin.height.saturating_sub(1) as usize;
            let first = idx.saturating_sub(visible.saturating_sub(1));
            if items.is_empty() {
                f.render_widget(
                    Span::styled("no matching key columns (only PK / UNIQUE columns can be referenced)", dim),
                    Rect::new(pin.x, pin.y + 1, pin.width, 1),
                );
            }
            for (i, item) in items.iter().enumerate().skip(first).take(visible) {
                let style = if i == idx { Style::default().fg(Color::Black).bg(FK_COLOR) } else { Style::default() };
                let text = format!("{:<w$}", format!(" {}  {}", item.label, item.ty), w = pin.width as usize);
                f.render_widget(Span::styled(text, style), Rect::new(pin.x, pin.y + 1 + (i - first) as u16, pin.width, 1));
            }
        }
    }
}

#[cfg(test)]
impl Draft {
    pub fn rect_of_for_test(&self, field: Field) -> Option<Rect> {
        self.rects.iter().find(|(_, f)| *f == field).map(|(r, _)| *r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        // one file per call: tests run in parallel
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = format!("/tmp/erd_test_designer_{}_{n}.db", std::process::id());
        crate::schema::test_db(&path);
        let s = crate::schema::load(&path, "_descriptions").unwrap();
        let _ = std::fs::remove_file(&path);
        s
    }

    fn press(d: &mut Draft, s: &Schema, code: KeyCode) -> bool {
        matches!(d.on_key(KeyEvent::new(code, KeyModifiers::NONE), s), Outcome::Submit)
    }

    #[test]
    fn default_values_are_quoted_sensibly() {
        assert_eq!(default_expr("0"), "0");
        assert_eq!(default_expr("-1.5"), "-1.5");
        assert_eq!(default_expr("open"), "'open'");
        assert_eq!(default_expr("it's"), "'it''s'");
        assert_eq!(default_expr("'x'"), "'x'");
        assert_eq!(default_expr("current_timestamp"), "current_timestamp");
        assert_eq!(default_expr("datetime('now')"), "(datetime('now'))");
    }

    #[test]
    fn generates_sql_with_everything() {
        let s = schema();
        let users = s.find_table("users").unwrap();
        let mut d = Draft::new("_descriptions");
        d.name = "orders".into();
        d.cols[0].autoinc = true;
        d.cols.push(DraftCol {
            name: "user_id".into(),
            ty: "INTEGER".into(),
            not_null: true,
            refs: Some(RefTarget::Table(ColRef { table: users, column: 0 })),
            on_delete: "CASCADE",
            ..Default::default()
        });
        d.cols.push(DraftCol { name: "parent_id".into(), ty: "INTEGER".into(), refs: Some(RefTarget::This(0)), on_update: "SET NULL", ..Default::default() });
        d.cols.push(DraftCol { name: "code".into(), ty: "TEXT".into(), default: "new".into(), collate: "NOCASE".into(), check: "length(code) > 1".into(), ..Default::default() });
        d.table_unique = "user_id, code".into();
        d.table_check = "parent_id <> id".into();
        d.indexes.push(DraftIndex { cols: "code DESC".into(), where_: "parent_id IS NULL".into(), ..Default::default() });
        let sql = d.sql(&s).unwrap();
        for part in [
            "    id INTEGER PRIMARY KEY AUTOINCREMENT",
            "user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE",
            "parent_id INTEGER REFERENCES orders(id) ON UPDATE SET NULL",
            "code TEXT DEFAULT 'new' COLLATE NOCASE CHECK (length(code) > 1)",
            "    UNIQUE (user_id, code)",
            "    CHECK (parent_id <> id)",
        ] {
            assert!(sql.contains(part), "{part}\n{sql}");
        }
        let ix = d.index_sqls(&s).unwrap();
        assert_eq!(ix, ["CREATE INDEX orders_code_idx ON orders (code DESC) WHERE parent_id IS NULL;"]);
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE users (id INTEGER PRIMARY KEY);").unwrap();
        c.execute_batch(&sql).unwrap();
        c.execute_batch(&ix[0]).unwrap();

        d.cols[1].autoinc = true;
        assert!(d.sql(&s).unwrap_err().contains("AUTOINCREMENT"));
    }

    #[test]
    fn composite_key_and_validation() {
        let s = schema();
        let mut d = Draft::new("_descriptions");
        d.name = "users".into();
        assert!(d.sql(&s).unwrap_err().contains("already exists"));
        d.name = "link".into();
        d.cols = vec![
            DraftCol { name: "a".into(), ty: "INTEGER".into(), pk: true, ..Default::default() },
            DraftCol { name: "b".into(), ty: "INTEGER".into(), pk: true, ..Default::default() },
        ];
        assert!(d.sql(&s).unwrap().contains("PRIMARY KEY (a, b)"));
        d.cols[1].name = "A".into();
        assert!(d.sql(&s).unwrap_err().contains("twice"));
        d.cols[1].name = String::new();
        assert!(d.sql(&s).unwrap_err().contains("needs a name"));
    }

    #[test]
    fn deleting_and_moving_rows_keeps_self_references_right() {
        let mut d = Draft::new("_descriptions");
        d.cols.push(DraftCol { name: "x".into(), ..Default::default() });
        d.cols.push(DraftCol { name: "parent".into(), refs: Some(RefTarget::This(0)), ..Default::default() });
        d.focus = Field::Col(0, ColField::Name);
        d.move_row(1); // id is now row 1
        assert_eq!(d.cols[2].refs, Some(RefTarget::This(1)));
        d.focus = Field::Col(0, ColField::Name);
        d.delete_row(); // remove "x": id back to row 0
        assert_eq!(d.cols[1].refs, Some(RefTarget::This(0)));
        d.focus = Field::Col(0, ColField::Name);
        d.delete_row(); // remove id: reference dropped
        assert_eq!(d.cols[0].refs, None);
    }

    #[test]
    fn arrows_add_and_remove_rows() {
        let s = schema();
        let mut d = Draft::new("_descriptions");
        d.name = "things".into();
        d.focus = Field::Col(0, ColField::Type);
        press(&mut d, &s, KeyCode::Down); // past the last column: a new one
        assert_eq!(d.cols.len(), 2);
        assert_eq!(d.focus, Field::Col(1, ColField::Name));
        press(&mut d, &s, KeyCode::Down); // leaving it empty: removed, on to the constraints
        assert_eq!(d.cols.len(), 1);
        assert_eq!(d.focus, Field::TableUnique);
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Down); // past CHECK: a new index
        assert_eq!(d.indexes.len(), 1);
        assert_eq!(d.focus, Field::Idx(0, IdxField::Cols));
        press(&mut d, &s, KeyCode::Up); // empty index removed again
        assert!(d.indexes.is_empty());
        assert_eq!(d.focus, Field::TableCheck);
        press(&mut d, &s, KeyCode::Up);
        press(&mut d, &s, KeyCode::Up); // back to the last column
        assert_eq!(d.focus, Field::Col(0, ColField::Name));
        press(&mut d, &s, KeyCode::Down);
        for c in "label".chars() {
            press(&mut d, &s, KeyCode::Char(c));
        }
        press(&mut d, &s, KeyCode::Up); // a filled-in row stays
        assert_eq!(d.cols.len(), 2);
        // choices open a list with space, backspace clears
        d.focus = Field::Col(1, ColField::OnDelete);
        press(&mut d, &s, KeyCode::Char(' '));
        assert!(d.picker.as_ref().is_some_and(|p| p.kind == PickerKind::Choice));
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Enter);
        assert!(d.picker.is_none());
        assert_eq!(d.cols[1].on_delete, "CASCADE");
        press(&mut d, &s, KeyCode::Char(' '));
        assert_eq!(d.picker.as_ref().unwrap().idx, 1, "the current choice is highlighted");
        press(&mut d, &s, KeyCode::Esc);
        assert!(d.picker.is_none());
        assert_eq!(d.cols[1].on_delete, "CASCADE", "esc keeps the value");
        press(&mut d, &s, KeyCode::Backspace);
        assert_eq!(d.cols[1].on_delete, "");
        assert!(press(&mut d, &s, KeyCode::Enter), "enter submits");
    }

    #[test]
    fn finds_what_the_form_cannot_show() {
        let f = unsupported_features(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, check_date TEXT, n INT CHECK (n > 0), \
             'strict' TEXT, total AS (n * 2), p INT REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED) STRICT",
        );
        assert_eq!(f, ["generated columns", "DEFERRABLE", "STRICT"]);
        assert!(unsupported_features("CREATE TABLE t (id INTEGER PRIMARY KEY, check_date TEXT, strict TEXT)").is_empty());
    }

    #[test]
    fn type_menu_asks_for_the_length() {
        let s = schema();
        let mut d = Draft::new("_descriptions");
        d.focus = Field::Col(0, ColField::Type);
        press(&mut d, &s, KeyCode::Char(' ')); // opens the type menu
        for c in "nvar".chars() {
            press(&mut d, &s, KeyCode::Char(c));
        }
        press(&mut d, &s, KeyCode::Enter); // NVARCHAR takes a number
        assert_eq!(d.picker.as_ref().unwrap().param.as_ref().unwrap().2, "255");
        for _ in 0..3 {
            press(&mut d, &s, KeyCode::Backspace);
        }
        press(&mut d, &s, KeyCode::Char('x')); // not a digit: ignored
        press(&mut d, &s, KeyCode::Char('1'));
        press(&mut d, &s, KeyCode::Char('5'));
        press(&mut d, &s, KeyCode::Enter);
        assert!(d.picker.is_none());
        assert_eq!(d.cols[0].ty, "NVARCHAR(15)");
        // reopening keeps the number; DECIMAL takes precision and scale
        press(&mut d, &s, KeyCode::Char(' '));
        for c in "nvar".chars() {
            press(&mut d, &s, KeyCode::Char(c));
        }
        press(&mut d, &s, KeyCode::Enter);
        assert_eq!(d.picker.as_ref().unwrap().param.as_ref().unwrap().2, "15");
        press(&mut d, &s, KeyCode::Esc); // back to the list
        press(&mut d, &s, KeyCode::Esc); // closed
        press(&mut d, &s, KeyCode::Char(' '));
        for c in "dec".chars() {
            press(&mut d, &s, KeyCode::Char(c));
        }
        press(&mut d, &s, KeyCode::Enter);
        press(&mut d, &s, KeyCode::Enter); // default 10,2
        assert_eq!(d.cols[0].ty, "DECIMAL(10,2)");
        // plain types need no number; typing still works in the field
        press(&mut d, &s, KeyCode::Char(' '));
        press(&mut d, &s, KeyCode::Enter); // first item: INTEGER
        assert_eq!(d.cols[0].ty, "INTEGER");
        press(&mut d, &s, KeyCode::Char('X'));
        assert_eq!(d.cols[0].ty, "INTEGERX");
    }

    fn screen(d: &mut Draft, s: &Schema) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        term.draw(|f| d.draw(f, f.area(), s)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..40).map(|y| (0..120).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
    }

    #[test]
    fn menus_show_notes() {
        let s = schema();
        let mut d = Draft::new("_descriptions");
        d.focus = Field::Col(0, ColField::Type);
        press(&mut d, &s, KeyCode::Char(' '));
        let text = screen(&mut d, &s);
        println!("{text}");
        for note in ["±9.2e18", "±32767", "Unicode", "all 64-bit", "~15 digits"] {
            assert!(text.contains(note), "{note}");
        }
        press(&mut d, &s, KeyCode::Esc);
        d.focus = Field::Col(0, ColField::OnDelete);
        press(&mut d, &s, KeyCode::Char(' '));
        let text = screen(&mut d, &s);
        println!("{text}");
        assert!(text.contains("delete this row too") && text.contains("error while still referenced"));
        press(&mut d, &s, KeyCode::Esc);
        d.focus = Field::Col(0, ColField::Collate);
        press(&mut d, &s, KeyCode::Char(' '));
        assert!(screen(&mut d, &s).contains("ignores ASCII case"));
    }

    #[test]
    fn index_columns_popup() {
        let s = schema();
        let mut d = Draft::new("_descriptions");
        d.name = "people".into();
        for n in ["last_name", "first_name", "email"] {
            d.cols.push(DraftCol { name: n.into(), ty: "TEXT".into(), ..Default::default() });
        }
        d.indexes.push(DraftIndex::default());
        d.focus = Field::Idx(0, IdxField::Cols);
        press(&mut d, &s, KeyCode::Char(' ')); // empty field: opens the list
        assert!(d.picker.as_ref().is_some_and(|p| p.kind == PickerKind::IndexCols));
        // tick last_name, then first_name (filtered), in that order
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Char(' '));
        for c in "fir".chars() {
            press(&mut d, &s, KeyCode::Char(c));
        }
        press(&mut d, &s, KeyCode::Char(' '));
        let text = screen(&mut d, &s);
        assert!(text.contains("index:  last_name, first_name"), "{text}");
        press(&mut d, &s, KeyCode::Enter);
        assert!(d.picker.is_none());
        assert_eq!(d.indexes[0].cols, "last_name, first_name");
        // in a filled field space types (a, b · name DESC); ctrl+space opens
        // the list, and DESC and expressions survive it
        d.indexes[0].cols = "last_name".into();
        for c in " DESC, lower(email)".chars() {
            press(&mut d, &s, KeyCode::Char(c));
        }
        assert!(d.picker.is_none());
        assert_eq!(d.indexes[0].cols, "last_name DESC, lower(email)");
        d.on_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL), &s);
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Char(' ')); // first_name
        press(&mut d, &s, KeyCode::Enter);
        assert_eq!(d.indexes[0].cols, "last_name DESC, lower(email), first_name");
        // unticking removes the column with its DESC; esc keeps the field
        d.indexes[0].cols = "last_name DESC, first_name".into();
        screen(&mut d, &s);
        let r = d.rect_of_for_test(Field::Idx(0, IdxField::Cols)).unwrap();
        d.on_click(r.x, r.y); // a click always opens it
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Char(' '));
        press(&mut d, &s, KeyCode::Esc);
        assert_eq!(d.indexes[0].cols, "last_name DESC, first_name");
        d.on_click(r.x, r.y);
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Char(' '));
        press(&mut d, &s, KeyCode::Enter);
        assert_eq!(d.indexes[0].cols, "first_name");
        assert_eq!(part_column("\"my col\" DESC").as_deref(), Some("my col"));
        assert_eq!(part_column("lower(email)"), None);
    }

    #[test]
    fn collate_colour_and_hints() {
        let s = schema();
        let mut d = Draft::new("_descriptions");
        d.focus = Field::Col(0, ColField::Collate);
        // space opens the list; Down + Enter picks the next one
        for want in ["BINARY", "NOCASE", "RTRIM"] {
            press(&mut d, &s, KeyCode::Char(' '));
            press(&mut d, &s, KeyCode::Down);
            press(&mut d, &s, KeyCode::Enter);
            assert_eq!(d.cols[0].collate, want);
        }
        // delete in the list clears
        press(&mut d, &s, KeyCode::Char(' '));
        press(&mut d, &s, KeyCode::Delete);
        assert!(d.picker.is_none());
        assert_eq!(d.cols[0].collate, "");
        // a typed letter opens the list filtered
        press(&mut d, &s, KeyCode::Char('n'));
        assert_eq!(d.picker.as_ref().unwrap().query, "n");
        press(&mut d, &s, KeyCode::Enter);
        assert_eq!(d.cols[0].collate, "NOCASE");
        // esc cancels
        press(&mut d, &s, KeyCode::Char('r'));
        press(&mut d, &s, KeyCode::Esc);
        assert_eq!(d.cols[0].collate, "NOCASE");
        assert_eq!(d.color, "white");
        d.focus = Field::Desc;
        press(&mut d, &s, KeyCode::Down);
        assert_eq!(d.focus, Field::Color);
        press(&mut d, &s, KeyCode::Char(' '));
        press(&mut d, &s, KeyCode::Down);
        press(&mut d, &s, KeyCode::Enter);
        assert_eq!(d.color, "red");
        for c in " blu".chars() {
            press(&mut d, &s, KeyCode::Char(c));
        }
        press(&mut d, &s, KeyCode::Enter);
        assert_eq!(d.color, "blue");
        press(&mut d, &s, KeyCode::Char(' '));
        press(&mut d, &s, KeyCode::Delete);
        assert_eq!(d.color, "white");
        press(&mut d, &s, KeyCode::Down);
        assert_eq!(d.focus, Field::Col(0, ColField::Name));
        for f in [ColField::Name, ColField::NotNull, ColField::Unique, ColField::Desc, ColField::AutoInc, ColField::Collate] {
            assert!(!d.hint(Field::Col(0, f)).is_empty(), "{f:?}");
        }
        assert!(d.hint(Field::Col(0, ColField::NotNull)).contains("NOT NULL"));
        // keys are bold, the rest is not
        let line = Draft::hint_line("*Space* toggles · x", Style::default());
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content, "[Space]");
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(!line.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }
}
