//! Everything that talks to the SQLite file: reading the schema, reading and
//! writing descriptions and the saved layout, and previewing table data.
//! The database is opened read-only except for the moment of a write.

use anyhow::{Context, Result};
use rusqlite::{params, types::ValueRef, Connection, OpenFlags};
use std::collections::HashMap;
use std::time::Duration;

/// Table storing manually arranged table positions.
pub const LAYOUT_TABLE: &str = "_erd_layout";
/// Table storing colours chosen for tables.
pub const COLORS_TABLE: &str = "_erd_colors";

/// Tables the tool keeps for itself (hidden from the diagram).
pub fn is_internal(name: &str, desc_table: &str) -> bool {
    name.eq_ignore_ascii_case(desc_table) || name.eq_ignore_ascii_case(LAYOUT_TABLE) || name.eq_ignore_ascii_case(COLORS_TABLE)
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    /// 1-based position in the primary key, 0 if not part of it.
    pub pk_index: usize,
    pub fk: bool,
    /// Target of at least one foreign key.
    pub referenced: bool,
    /// Covered on its own by a UNIQUE index or constraint.
    pub unique: bool,
    pub collate: Option<String>,
    /// CHECK expression(s) written on the column
    pub check: Option<String>,
    pub autoincrement: bool,
    pub description: Option<String>,
}

impl Column {
    pub fn is_pk(&self) -> bool {
        self.pk_index > 0
    }
}

#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    pub unique: bool,
    /// "c" = CREATE INDEX, "u" = UNIQUE constraint, "pk" = PRIMARY KEY
    pub origin: String,
    pub columns: Vec<String>,
    pub sql: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub description: Option<String>,
    pub columns: Vec<Column>,
    pub sql: Option<String>,
    pub indexes: Vec<Index>,
    /// has a foreign key spanning several columns (not editable in the designer)
    pub composite_fks: bool,
    pub row_count: Option<i64>,
    /// table-level CHECK expressions and UNIQUE column lists
    pub checks: Vec<String>,
    pub uniques: Vec<String>,
    /// parts of the CREATE TABLE that couldn't be read
    pub unread: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColRef {
    pub table: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForeignKey {
    pub from: ColRef,
    pub to: ColRef,
    /// "" when no action is set (NO ACTION)
    pub on_delete: &'static str,
    pub on_update: &'static str,
}

/// Foreign key actions the editor offers ("" = none / NO ACTION).
pub const FK_ACTIONS: [&str; 5] = ["", "CASCADE", "SET NULL", "SET DEFAULT", "RESTRICT"];

fn fk_action(s: &str) -> &'static str {
    FK_ACTIONS.iter().find(|a| !a.is_empty() && a.eq_ignore_ascii_case(s)).copied().unwrap_or("")
}

pub struct Schema {
    pub tables: Vec<Table>,
    pub fks: Vec<ForeignKey>,
    /// views and triggers: (type, name, sql), for the SQL export
    pub others: Vec<(String, String, String)>,
    pub warnings: Vec<String>,
}

impl Schema {
    pub fn col(&self, c: ColRef) -> &Column {
        &self.tables[c.table].columns[c.column]
    }

    pub fn qualified(&self, c: ColRef) -> String {
        format!("{}.{}", self.tables[c.table].name, self.col(c).name)
    }

    pub fn find_table(&self, name: &str) -> Option<usize> {
        self.tables.iter().position(|t| t.name.eq_ignore_ascii_case(name))
    }
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

const KEYWORDS: &[&str] = &[
    "ABORT", "ACTION", "ADD", "AFTER", "ALL", "ALTER", "ALWAYS", "ANALYZE", "AND", "AS", "ASC", "ATTACH",
    "AUTOINCREMENT", "BEFORE", "BEGIN", "BETWEEN", "BY", "CASCADE", "CASE", "CAST", "CHECK", "COLLATE",
    "COLUMN", "COMMIT", "CONFLICT", "CONSTRAINT", "CREATE", "CROSS", "CURRENT", "CURRENT_DATE",
    "CURRENT_TIME", "CURRENT_TIMESTAMP", "DATABASE", "DEFAULT", "DEFERRABLE", "DEFERRED", "DELETE", "DESC",
    "DETACH", "DISTINCT", "DO", "DROP", "EACH", "ELSE", "END", "ESCAPE", "EXCEPT", "EXCLUDE", "EXCLUSIVE",
    "EXISTS", "EXPLAIN", "FAIL", "FILTER", "FIRST", "FOLLOWING", "FOR", "FOREIGN", "FROM", "FULL",
    "GENERATED", "GLOB", "GROUP", "GROUPS", "HAVING", "IF", "IGNORE", "IMMEDIATE", "IN", "INDEX", "INDEXED",
    "INITIALLY", "INNER", "INSERT", "INSTEAD", "INTERSECT", "INTO", "IS", "ISNULL", "JOIN", "KEY", "LAST",
    "LEFT", "LIKE", "LIMIT", "MATCH", "MATERIALIZED", "NATURAL", "NO", "NOT", "NOTHING", "NOTNULL", "NULL",
    "NULLS", "OF", "OFFSET", "ON", "OR", "ORDER", "OTHERS", "OUTER", "OVER", "PARTITION", "PLAN", "PRAGMA",
    "PRECEDING", "PRIMARY", "QUERY", "RAISE", "RANGE", "RECURSIVE", "REFERENCES", "REGEXP", "REINDEX",
    "RELEASE", "RENAME", "REPLACE", "RESTRICT", "RETURNING", "RIGHT", "ROLLBACK", "ROW", "ROWS", "SAVEPOINT",
    "SELECT", "SET", "TABLE", "TEMP", "TEMPORARY", "THEN", "TIES", "TO", "TRANSACTION", "TRIGGER",
    "UNBOUNDED", "UNION", "UNIQUE", "UPDATE", "USING", "VACUUM", "VALUES", "VIEW", "VIRTUAL", "WHEN", "WHERE",
    "WINDOW", "WITH", "WITHOUT",
];

/// Plain identifiers (letters, digits, _; not a keyword) don't need quotes.
pub fn needs_quotes(s: &str) -> bool {
    let mut chars = s.chars();
    let plain = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    !plain || KEYWORDS.contains(&s.to_ascii_uppercase().as_str())
}

/// An identifier as it should appear in SQL written for people:
/// bare when possible, quoted only when SQL requires it.
pub fn ident_sql(s: &str) -> String {
    if needs_quotes(s) { quote_ident(s) } else { s.to_string() }
}

fn value_to_string(v: ValueRef) -> Option<String> {
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Some(format!("<blob {} B>", b.len())),
    }
}

fn find_col(t: &Table, name: &str) -> Option<usize> {
    t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
}

fn open_ro(path: &str) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {path}"))
}

fn open_rw(path: &str) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("cannot open {path} for writing"))?;
    conn.busy_timeout(Duration::from_secs(3))?;
    Ok(conn)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND lower(name) = lower(?1)",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// A fresh copy of the test database (src/test_schema.sql) at `path`.
#[cfg(test)]
pub fn test_db(path: &str) {
    let _ = std::fs::remove_file(path);
    Connection::open(path).unwrap().execute_batch(include_str!("test_schema.sql")).unwrap();
}

// ------------------------------------------------------------------- load

pub fn load(path: &str, desc_table: &str) -> Result<Schema> {
    let conn = open_ro(path)?;
    let mut warnings = Vec::new();

    let all: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT name, sql FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
             ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let descs = if all.iter().any(|(n, _)| n.eq_ignore_ascii_case(desc_table)) {
        load_descriptions(&conn, desc_table).unwrap_or_else(|e| {
            warnings.push(format!(
                "could not read descriptions from '{desc_table}' \
                 (expected columns table_name, column_name, description): {e}"
            ));
            HashMap::new()
        })
    } else {
        HashMap::new()
    };

    let mut tables = Vec::new();
    for (name, sql) in all
        .into_iter()
        .filter(|(n, _)| !is_internal(n, desc_table))
    {
        let mut stmt = conn.prepare(
            "SELECT name, type, \"notnull\", dflt_value, pk
             FROM pragma_table_info(?1) ORDER BY cid",
        )?;
        let columns = stmt
            .query_map([&name], |r| {
                Ok(Column {
                    name: r.get(0)?,
                    data_type: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    not_null: r.get::<_, i64>(2)? != 0,
                    default: value_to_string(r.get_ref(3)?),
                    pk_index: r.get::<_, i64>(4)?.max(0) as usize,
                    fk: false,
                    referenced: false,
                    unique: false,
                    collate: None,
                    check: None,
                    autoincrement: false,
                    description: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let key = name.to_lowercase();
        let parsed = crate::sqlparse::parse_create_table(sql.as_deref().unwrap_or(""));
        let mut columns = columns;
        for c in &mut columns {
            if let Some(ex) = parsed.columns.get(&c.name.to_lowercase()) {
                c.collate = ex.collate.clone();
                c.autoincrement = ex.autoincrement;
                c.check = match ex.checks.len() {
                    0 => None,
                    1 => Some(ex.checks[0].clone()),
                    _ => Some(ex.checks.iter().map(|x| format!("({x})")).collect::<Vec<_>>().join(" AND ")),
                };
            }
        }
        let row_count = conn
            .query_row(&format!("SELECT COUNT(*) FROM {}", quote_ident(&name)), [], |r| r.get(0))
            .ok();
        let mut table = Table {
            description: descs.get(&(key.clone(), String::new())).cloned(),
            indexes: load_indexes(&conn, &name)?,
            composite_fks: false,
            row_count,
            checks: parsed.table_checks,
            uniques: parsed.table_uniques,
            unread: parsed.unread,
            name,
            columns,
            sql,
        };
        for c in &mut table.columns {
            c.description = descs.get(&(key.clone(), c.name.to_lowercase())).cloned();
        }
        let unique_cols: Vec<String> = table
            .indexes
            .iter()
            .filter(|i| i.unique && i.columns.len() == 1)
            .map(|i| i.columns[0].to_lowercase())
            .collect();
        for c in &mut table.columns {
            c.unique = unique_cols.contains(&c.name.to_lowercase());
        }
        tables.push(table);
    }

    let index: HashMap<String, usize> =
        tables.iter().enumerate().map(|(i, t)| (t.name.to_lowercase(), i)).collect();

    let mut fks = Vec::new();
    for ti in 0..tables.len() {
        let mut stmt = conn.prepare(
            "SELECT seq, \"table\", \"from\", \"to\", on_delete, on_update
             FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
        )?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, String, String, Option<String>, String, String)> = stmt
            .query_map([&tables[ti].name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;

        if rows.iter().any(|r| r.0 > 0) {
            tables[ti].composite_fks = true;
        }
        for (seq, target, from, to, on_delete, on_update) in rows {
            let tname = tables[ti].name.clone();
            let Some(from_c) = find_col(&tables[ti], &from) else {
                warnings.push(format!("{tname}: foreign key column '{from}' not found"));
                continue;
            };
            tables[ti].columns[from_c].fk = true;

            let Some(&tt) = index.get(&target.to_lowercase()) else {
                warnings.push(format!("{tname}.{from} references missing table '{target}'"));
                continue;
            };
            let to_c = match &to {
                Some(to) => find_col(&tables[tt], to),
                None => {
                    // REFERENCES target without column list => target's primary key
                    let mut pks: Vec<(usize, usize)> = tables[tt]
                        .columns
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.is_pk())
                        .map(|(i, c)| (c.pk_index, i))
                        .collect();
                    pks.sort();
                    pks.get(seq as usize).map(|p| p.1)
                }
            };
            let Some(to_c) = to_c else {
                warnings.push(format!("{tname}.{from} references unknown column in '{target}'"));
                continue;
            };
            tables[tt].columns[to_c].referenced = true;
            fks.push(ForeignKey {
                from: ColRef { table: ti, column: from_c },
                to: ColRef { table: tt, column: to_c },
                on_delete: fk_action(&on_delete),
                on_update: fk_action(&on_update),
            });
        }
    }

    let others = {
        let mut stmt = conn.prepare(
            "SELECT type, name, sql FROM sqlite_master
             WHERE type IN ('view', 'trigger') AND sql IS NOT NULL
             ORDER BY type = 'trigger', name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(Schema { tables, fks, others, warnings })
}

fn load_indexes(conn: &Connection, table: &str) -> Result<Vec<Index>> {
    let mut stmt = conn.prepare(
        "SELECT il.name, il.\"unique\", il.origin, m.sql
         FROM pragma_index_list(?1) il
         LEFT JOIN sqlite_master m ON m.type = 'index' AND m.name = il.name
         ORDER BY il.seq DESC",
    )?;
    let list: Vec<(String, bool, String, Option<String>)> = stmt
        .query_map([table], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::new();
    for (name, unique, origin, sql) in list {
        let mut stmt = conn.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
        let columns = stmt
            .query_map([&name], |r| Ok(r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "<expr>".into())))?
            .collect::<rusqlite::Result<_>>()?;
        out.push(Index { name, unique, origin, columns, sql });
    }
    Ok(out)
}

/// Key: (table name lowercase, column name lowercase or "" for the table itself).
fn load_descriptions(conn: &Connection, table: &str) -> rusqlite::Result<HashMap<(String, String), String>> {
    let sql = format!("SELECT table_name, column_name, description FROM {}", quote_ident(table));
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut map = HashMap::new();
    while let Some(r) = rows.next()? {
        let t: Option<String> = r.get(0)?;
        let c: Option<String> = r.get(1)?;
        let d: Option<String> = r.get(2)?;
        if let (Some(t), Some(d)) = (t, d) {
            let d = d.trim().to_string();
            if !d.is_empty() {
                map.insert((t.to_lowercase(), c.unwrap_or_default().to_lowercase()), d);
            }
        }
    }
    Ok(map)
}

// ----------------------------------------------------------- descriptions

fn ensure_desc_table(conn: &Connection, desc_table: &str) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {} (
                table_name  TEXT NOT NULL,
                column_name TEXT,
                description TEXT NOT NULL
            )",
            quote_ident(desc_table)
        ),
        [],
    )?;
    Ok(())
}

/// Replace one description (removed when `text` is empty).
fn put_description(conn: &Connection, desc_table: &str, table: &str, column: Option<&str>, text: &str) -> rusqlite::Result<()> {
    let q = quote_ident(desc_table);
    match column {
        Some(c) => conn.execute(
            &format!("DELETE FROM {q} WHERE lower(table_name) = lower(?1) AND lower(column_name) = lower(?2)"),
            params![table, c],
        )?,
        None => conn.execute(
            &format!("DELETE FROM {q} WHERE lower(table_name) = lower(?1) AND (column_name IS NULL OR column_name = '')"),
            params![table],
        )?,
    };
    let text = text.trim();
    if !text.is_empty() {
        conn.execute(
            &format!("INSERT INTO {q} (table_name, column_name, description) VALUES (?1, ?2, ?3)"),
            params![table, column, text],
        )?;
    }
    Ok(())
}

/// Write (or with empty `text`, remove) one description. Creates the
/// descriptions table if it doesn't exist yet.
pub fn save_description(path: &str, desc_table: &str, table: &str, column: Option<&str>, text: &str) -> Result<()> {
    let mut conn = open_rw(path)?;
    let tx = conn.transaction()?;
    ensure_desc_table(&tx, desc_table)?;
    put_description(&tx, desc_table, table, column, text)?;
    tx.commit()?;
    Ok(())
}

/// Run a CREATE TABLE statement and store the new table's descriptions,
/// all in one transaction: either everything is created or nothing.
pub struct NewTable<'a> {
    pub name: &'a str,
    pub sql: &'a str,
    pub index_sqls: &'a [String],
    /// copy all rows from this table (duplicating with data)
    pub copy_rows_from: Option<(&'a str, &'a [String])>,
    pub desc_table: &'a str,
    pub table_desc: &'a str,
    pub column_descs: &'a [(String, String)],
    /// colour in the diagram; None or the default (first) colour = none stored
    pub color: Option<&'a str>,
}

/// Create a table with its indexes and descriptions (and optionally rows
/// copied from another table), all in one transaction.
pub fn create_table(path: &str, t: &NewTable) -> Result<()> {
    let (sql, table, desc_table, table_desc, column_descs) = (t.sql, t.name, t.desc_table, t.table_desc, t.column_descs);
    let mut conn = open_rw(path)?;
    let tx = conn.transaction()?;
    tx.execute_batch(sql)?;
    for ix in t.index_sqls {
        tx.execute_batch(ix).with_context(|| format!("creating index: {ix}"))?;
    }
    if let Some((from, cols)) = t.copy_rows_from {
        let cols: Vec<String> = cols.iter().map(|c| quote_ident(c)).collect();
        let cols = cols.join(", ");
        tx.execute_batch(&format!("INSERT INTO {} ({cols}) SELECT {cols} FROM {}", quote_ident(table), quote_ident(from)))
            .context("copying the rows")?;
    }
    put_color(&tx, table, t.color)?;
    let has_descs = !table_desc.trim().is_empty() || column_descs.iter().any(|(_, d)| !d.trim().is_empty());
    if has_descs {
        ensure_desc_table(&tx, desc_table)?;
        put_description(&tx, desc_table, table, None, table_desc)?;
        for (col, d) in column_descs {
            put_description(&tx, desc_table, table, Some(col), d)?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Replace all descriptions of a table (also under its old name).
fn replace_descriptions(
    conn: &Connection,
    desc_table: &str,
    old_name: &str,
    new_name: &str,
    table_desc: &str,
    column_descs: &[(String, String)],
) -> Result<()> {
    if table_exists(conn, desc_table)? {
        conn.execute(
            &format!("DELETE FROM {} WHERE lower(table_name) IN (lower(?1), lower(?2))", quote_ident(desc_table)),
            params![old_name, new_name],
        )?;
    }
    let any = !table_desc.trim().is_empty() || column_descs.iter().any(|(_, d)| !d.trim().is_empty());
    if any {
        ensure_desc_table(conn, desc_table)?;
        put_description(conn, desc_table, new_name, None, table_desc)?;
        for (c, d) in column_descs {
            put_description(conn, desc_table, new_name, Some(c), d)?;
        }
    }
    Ok(())
}

/// Temporary name used while rebuilding a table.
pub const REBUILD_TMP: &str = "_erd_rebuild_tmp";

/// A change to an existing table: any mix of renames, a rebuild (for what
/// ALTER TABLE can't do in place), a new set of indexes and descriptions.
pub struct TableChange<'a> {
    pub old_name: &'a str,
    pub new_name: &'a str,
    /// column renames (old name, new name), done with RENAME COLUMN so
    /// indexes, views and other tables' foreign keys follow along
    pub renames: &'a [(String, String)],
    /// columns that go away
    pub dropped: &'a [String],
    /// CREATE TABLE for the new definition, named REBUILD_TMP, and the
    /// columns (new names) whose data is copied over; None = no rebuild
    pub rebuild: Option<(&'a str, &'a [String])>,
    /// the table's CREATE INDEX statements afterwards; None = leave them
    pub indexes: Option<&'a [String]>,
    pub desc_table: &'a str,
    pub table_desc: &'a str,
    pub column_descs: &'a [(String, String)],
    /// the table's colour afterwards (None or the default = none stored)
    pub color: Option<&'a str>,
}

/// Apply a table change following the procedure from the SQLite docs
/// (https://sqlite.org/lang_altertable.html#otheralter), in one
/// transaction: if anything fails, nothing is changed.
pub fn apply_table_change(path: &str, spec: &TableChange) -> Result<()> {
    let mut conn = open_rw(path)?;
    let fk_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")?; // no effect inside a transaction
    let result = change_table(&mut conn, spec);
    if fk_on == 1 {
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
    }
    result
}

fn change_table(conn: &mut Connection, spec: &TableChange) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch("PRAGMA legacy_alter_table = OFF")?;
    let exec = |sql: &str, what: &str| tx.execute_batch(sql).with_context(|| what.to_string());

    // 1. renames first, so everything that points at the table follows along
    let mut cur = spec.old_name.to_string();
    if spec.old_name != spec.new_name {
        if spec.old_name.eq_ignore_ascii_case(spec.new_name) {
            // SQLite refuses a rename that only changes upper/lower case
            exec(&format!("ALTER TABLE {} RENAME TO {}", quote_ident(&cur), quote_ident("_erd_rename_tmp")), "renaming the table")?;
            cur = "_erd_rename_tmp".into();
        }
        exec(&format!("ALTER TABLE {} RENAME TO {}", quote_ident(&cur), quote_ident(spec.new_name)), "renaming the table")?;
        cur = spec.new_name.to_string();
    }
    // a column that goes away may be in the way of a rename (a → b while b is removed)
    for (i, d) in spec.dropped.iter().enumerate() {
        if spec.renames.iter().any(|(_, new)| new.eq_ignore_ascii_case(d)) {
            exec(
                &format!("ALTER TABLE {} RENAME COLUMN {} TO {}", quote_ident(&cur), quote_ident(d), quote_ident(&format!("_erd_drop_{i}"))),
                &format!("moving column {d} out of the way"),
            )?;
        }
    }
    // two steps via temporary names, so swaps and case changes work
    for (i, (old, _)) in spec.renames.iter().enumerate() {
        exec(
            &format!("ALTER TABLE {} RENAME COLUMN {} TO {}", quote_ident(&cur), quote_ident(old), quote_ident(&format!("_erd_col_{i}"))),
            &format!("renaming column {old}"),
        )?;
    }
    for (i, (_, new)) in spec.renames.iter().enumerate() {
        exec(
            &format!("ALTER TABLE {} RENAME COLUMN {} TO {}", quote_ident(&cur), quote_ident(&format!("_erd_col_{i}")), quote_ident(new)),
            &format!("renaming column to {new}"),
        )?;
    }

    // 2. a new set of indexes replaces the old one
    if spec.indexes.is_some() {
        let old: Vec<String> = {
            let mut st = tx.prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL")?;
            let rows = st.query_map([&cur], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for name in old {
            exec(&format!("DROP INDEX {}", quote_ident(&name)), &format!("dropping index {name}"))?;
        }
    }

    // 3. rebuild: new table, copy, swap
    if let Some((create_tmp_sql, keep)) = spec.rebuild {
        let attached: Vec<(String, String, String)> = {
            let mut st = tx.prepare(
                "SELECT type, name, sql FROM sqlite_master
                 WHERE tbl_name = ?1 AND type IN ('index', 'trigger') AND sql IS NOT NULL",
            )?;
            let rows = st.query_map([&cur], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let views: Vec<(String, String)> = {
            let mut st = tx.prepare("SELECT name, sql FROM sqlite_master WHERE type = 'view' AND sql IS NOT NULL")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (name, _) in &views {
            exec(&format!("DROP VIEW {}", quote_ident(name)), "dropping views temporarily")?;
        }
        exec(create_tmp_sql, "creating the new table definition")?;
        if !keep.is_empty() {
            let cols: Vec<String> = keep.iter().map(|c| quote_ident(c)).collect();
            let cols = cols.join(", ");
            tx.execute_batch(&format!("INSERT INTO {} ({cols}) SELECT {cols} FROM {}", quote_ident(REBUILD_TMP), quote_ident(&cur)))
                .map_err(|e| {
                    let m = e.to_string();
                    let hint = if m.contains("NOT NULL") {
                        " (existing rows have no value for a new NOT NULL column: give it a default)"
                    } else if m.contains("UNIQUE") {
                        " (existing rows have duplicate values where they now must be unique)"
                    } else if m.contains("CHECK") {
                        " (existing rows don't pass the new CHECK)"
                    } else {
                        ""
                    };
                    anyhow::anyhow!("copying the existing rows failed: {e}{hint}")
                })?;
        }
        exec(&format!("DROP TABLE {}", quote_ident(&cur)), "dropping the old table")?;
        // legacy mode: rename without rewriting or re-checking other objects
        tx.execute_batch("PRAGMA legacy_alter_table = ON")?;
        exec(&format!("ALTER TABLE {} RENAME TO {}", quote_ident(REBUILD_TMP), quote_ident(&cur)), "renaming the new table into place")?;
        tx.execute_batch("PRAGMA legacy_alter_table = OFF")?;
        for (kind, name, sql) in &attached {
            exec(sql, &format!("recreating {kind} {name} (does it use a removed column?)"))?;
        }
        for (name, sql) in &views {
            exec(sql, &format!("recreating view {name} (does it use a removed column?)"))?;
        }
        let bad: Vec<(i64, String)> = {
            let mut st = tx.prepare("SELECT rowid, parent FROM pragma_foreign_key_check(?1)")?;
            let rows = st.query_map([&cur], |r| Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if let Some((rowid, parent)) = bad.first() {
            anyhow::bail!(
                "{} existing row(s) point to rows that don't exist in {parent} (first: rowid {rowid}); nothing was changed",
                bad.len()
            );
        }
    }

    // 4. indexes (after a rebuild the new columns exist)
    if let Some(indexes) = spec.indexes {
        for ix in indexes {
            exec(ix, &format!("creating index: {ix}"))?;
        }
    }

    // 5. descriptions, layout and colour follow the new names
    replace_descriptions(&tx, spec.desc_table, spec.old_name, &cur, spec.table_desc, spec.column_descs)?;
    if spec.old_name != cur {
        for meta in [LAYOUT_TABLE, COLORS_TABLE] {
            if table_exists(&tx, meta)? {
                tx.execute(
                    &format!("UPDATE {} SET table_name = ?1 WHERE lower(table_name) = lower(?2)", quote_ident(meta)),
                    params![&cur, spec.old_name],
                )?;
            }
        }
    }
    put_color(&tx, &cur, spec.color)?;
    tx.commit()?;
    Ok(())
}

/// Drop a table, with its descriptions, saved position and colour.
pub fn delete_table(path: &str, table: &str, desc_table: &str) -> Result<()> {
    let mut conn = open_rw(path)?;
    let fk_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF")?; // don't cascade into other tables
    let result = (|| -> Result<()> {
        let tx = conn.transaction()?;
        tx.execute_batch(&format!("DROP TABLE {}", quote_ident(table)))?;
        for meta in [desc_table, LAYOUT_TABLE, COLORS_TABLE] {
            if table_exists(&tx, meta)? {
                tx.execute(&format!("DELETE FROM {} WHERE lower(table_name) = lower(?1)", quote_ident(meta)), [table])?;
            }
        }
        tx.commit()?;
        Ok(())
    })();
    if fk_on == 1 {
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
    }
    result
}

// ----------------------------------------------------------------- colours

/// Chosen table colours, keyed by lowercase table name.
pub fn load_colors(path: &str) -> Result<HashMap<String, String>> {
    let conn = open_ro(path)?;
    let mut map = HashMap::new();
    if !table_exists(&conn, COLORS_TABLE)? {
        return Ok(map);
    }
    let mut st = conn.prepare(&format!("SELECT table_name, color FROM {}", quote_ident(COLORS_TABLE)))?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (t, c) = row?;
        map.insert(t.to_lowercase(), c);
    }
    Ok(map)
}

/// Store a table's colour; None, or the default (first) colour, stores nothing.
fn put_color(conn: &Connection, table: &str, color: Option<&str>) -> Result<()> {
    let q = quote_ident(COLORS_TABLE);
    let color = color.filter(|c| !c.eq_ignore_ascii_case(crate::diagram::TABLE_COLORS[0].0));
    if table_exists(conn, COLORS_TABLE)? {
        conn.execute(&format!("DELETE FROM {q} WHERE lower(table_name) = lower(?1)"), [table])?;
    }
    if let Some(c) = color {
        conn.execute(&format!("CREATE TABLE IF NOT EXISTS {q} (table_name TEXT PRIMARY KEY, color TEXT NOT NULL)"), [])?;
        conn.execute(&format!("INSERT INTO {q} (table_name, color) VALUES (?1, ?2)"), params![table, c])?;
    }
    Ok(())
}

pub fn save_color(path: &str, table: &str, color: Option<&str>) -> Result<()> {
    let mut conn = open_rw(path)?;
    let tx = conn.transaction()?;
    put_color(&tx, table, color)?;
    tx.commit()?;
    Ok(())
}

// ------------------------------------------------------------------ query

pub struct QueryResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// more rows than were fetched
    pub truncated: bool,
}

/// Run one statement on a read-only connection: nothing can be changed.
pub fn run_query(path: &str, sql: &str, limit: usize) -> Result<QueryResult> {
    let conn = open_ro(path)?;
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        anyhow::bail!("type a query, e.g. SELECT * FROM some_table");
    }
    let mut stmt = conn.prepare(sql).map_err(|e| anyhow::anyhow!("{e}"))?;
    let headers: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let n = headers.len();
    let mut rows = stmt.query([]).map_err(|e| {
        let m = e.to_string();
        if m.contains("readonly") {
            anyhow::anyhow!("the console is read-only: it runs queries, not changes")
        } else {
            anyhow::anyhow!("{m}")
        }
    })?;
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(r) = rows.next()? {
        if out.len() == limit {
            truncated = true;
            break;
        }
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            v.push(value_to_string(r.get_ref(i)?).unwrap_or_else(|| "NULL".into()));
        }
        out.push(v);
    }
    Ok(QueryResult { headers, rows: out, truncated })
}

/// Rows breaking a foreign key: (table, parent table, count), or the
/// error SQLite gives when a foreign key itself is broken.
pub fn orphans(path: &str) -> Result<Vec<(String, String, i64)>> {
    let conn = open_ro(path)?;
    let mut st = conn.prepare(
        "SELECT \"table\", parent, COUNT(*) FROM pragma_foreign_key_check GROUP BY \"table\", parent ORDER BY 1, 2",
    )?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Last-modified stamp of the database and its WAL file, to notice changes.
pub fn file_stamp(path: &str) -> Option<(std::time::SystemTime, u64, Option<std::time::SystemTime>)> {
    let m = std::fs::metadata(path).ok()?;
    let wal = std::fs::metadata(format!("{path}-wal")).ok().and_then(|w| w.modified().ok());
    Some((m.modified().ok()?, m.len(), wal))
}

// ----------------------------------------------------------------- layout

/// Saved grid positions, keyed by lowercase table name.
pub fn load_layout(path: &str) -> Result<HashMap<String, (usize, usize)>> {
    let conn = open_ro(path)?;
    let mut map = HashMap::new();
    if !table_exists(&conn, LAYOUT_TABLE)? {
        return Ok(map);
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT table_name, grid_col, grid_row FROM {}",
        quote_ident(LAYOUT_TABLE)
    ))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
    for row in rows {
        let (t, c, r) = row?;
        if c >= 0 && r >= 0 {
            map.insert(t.to_lowercase(), (c as usize, r as usize));
        }
    }
    Ok(map)
}

pub fn save_layout(path: &str, entries: &[(String, usize, usize)]) -> Result<()> {
    let mut conn = open_rw(path)?;
    let q = quote_ident(LAYOUT_TABLE);
    let tx = conn.transaction()?;
    tx.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {q} (
                table_name TEXT PRIMARY KEY,
                grid_col   INTEGER NOT NULL,
                grid_row   INTEGER NOT NULL
            )"
        ),
        [],
    )?;
    tx.execute(&format!("DELETE FROM {q}"), [])?;
    for (t, c, r) in entries {
        tx.execute(
            &format!("INSERT INTO {q} (table_name, grid_col, grid_row) VALUES (?1, ?2, ?3)"),
            params![t, *c as i64, *r as i64],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn clear_layout(path: &str) -> Result<()> {
    let conn = open_rw(path)?;
    conn.execute(&format!("DROP TABLE IF EXISTS {}", quote_ident(LAYOUT_TABLE)), [])?;
    Ok(())
}

// ---------------------------------------------------------------- preview

pub struct Preview {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total: i64,
}

pub fn preview(path: &str, table: &str, limit: usize) -> Result<Preview> {
    let conn = open_ro(path)?;
    let q = quote_ident(table);
    let total: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {q}"), [], |r| r.get(0))?;
    let mut stmt = conn.prepare(&format!("SELECT * FROM {q} LIMIT {limit}"))?;
    let headers: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let n = headers.len();
    let mut out = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            v.push(value_to_string(r.get_ref(i)?).unwrap_or_else(|| "NULL".into()));
        }
        out.push(v);
    }
    Ok(Preview { headers, rows: out, total })
}
