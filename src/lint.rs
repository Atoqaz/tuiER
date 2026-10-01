//! Schema checks: broken references, missing indexes on foreign keys,
//! tables without a primary key, and similar.

use crate::diagram::Selection;
use crate::schema::{self, ColRef, Schema};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

pub struct Issue {
    pub severity: Severity,
    pub message: String,
    pub target: Option<Selection>,
}

/// SQLite's type affinity rules (https://sqlite.org/datatype3.html#determination_of_column_affinity).
fn affinity(ty: &str) -> &'static str {
    let t = ty.to_uppercase();
    if t.contains("INT") {
        "INTEGER"
    } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        "TEXT"
    } else if t.contains("BLOB") || t.is_empty() {
        "BLOB"
    } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
        "REAL"
    } else {
        "NUMERIC"
    }
}

pub fn run(s: &Schema, path: &str) -> Vec<Issue> {
    let mut v = Vec::new();
    let mut add = |severity, message: String, target| v.push(Issue { severity, message, target });

    match schema::orphans(path) {
        Ok(rows) => {
            for (t, parent, n) in rows {
                add(
                    Severity::Error,
                    format!("{n} row(s) in {t} point to rows that don't exist in {parent}"),
                    s.find_table(&t).map(Selection::Table),
                );
            }
        }
        Err(e) => add(Severity::Error, format!("the foreign keys can't be checked: {e}"), None),
    }

    for fk in &s.fks {
        let (from, to) = (s.col(fk.from), s.col(fk.to));
        let (from_name, to_name) = (s.qualified(fk.from), s.qualified(fk.to));
        let target = Some(Selection::Column(fk.from));
        let parent = &s.tables[fk.to.table];
        let pk_count = parent.columns.iter().filter(|c| c.is_pk()).count();
        if !((to.is_pk() && pk_count == 1) || to.unique) {
            add(
                Severity::Error,
                format!("{from_name} references {to_name}, which is neither a single primary key nor UNIQUE: writes fail with \"foreign key mismatch\""),
                target,
            );
        }
        if affinity(&from.data_type) != affinity(&to.data_type) {
            add(
                Severity::Warning,
                format!(
                    "{from_name} is {} but {to_name} is {}: equal-looking values may not match",
                    if from.data_type.is_empty() { "untyped" } else { &from.data_type },
                    if to.data_type.is_empty() { "untyped" } else { &to.data_type }
                ),
                target,
            );
        }
        let child = &s.tables[fk.from.table];
        let child_pks = child.columns.iter().filter(|c| c.is_pk()).count();
        let indexed = (from.is_pk() && child_pks == 1)
            || child.indexes.iter().any(|i| i.columns.first().is_some_and(|c| c.eq_ignore_ascii_case(&from.name)));
        if !indexed {
            add(
                Severity::Warning,
                format!("{from_name} has no index: finding its rows, and deleting from {}, scans all of {}", parent.name, child.name),
                target,
            );
        }
    }

    for (ti, t) in s.tables.iter().enumerate() {
        if !t.columns.iter().any(|c| c.is_pk()) {
            add(Severity::Warning, format!("{} has no primary key: rows can't be referenced reliably", t.name), Some(Selection::Table(ti)));
        }
        for (ci, c) in t.columns.iter().enumerate() {
            if c.data_type.trim().is_empty() {
                add(Severity::Info, format!("{}.{} has no type", t.name, c.name), Some(Selection::Column(ColRef { table: ti, column: ci })));
            }
        }
        for (i, a) in t.indexes.iter().enumerate() {
            for b in &t.indexes[i + 1..] {
                let same = a.columns.len() == b.columns.len() && a.columns.iter().zip(&b.columns).all(|(x, y)| x.eq_ignore_ascii_case(y));
                if same {
                    add(Severity::Warning, format!("{}: {} and {} index the same columns", t.name, a.name, b.name), Some(Selection::Table(ti)));
                }
            }
        }
    }

    let no_desc_tables = s.tables.iter().filter(|t| t.description.is_none()).count();
    let no_desc_cols: usize = s.tables.iter().map(|t| t.columns.iter().filter(|c| c.description.is_none()).count()).sum();
    if no_desc_tables + no_desc_cols > 0 {
        add(
            Severity::Info,
            format!("{no_desc_tables} table(s) and {no_desc_cols} column(s) have no description (n walks through them)"),
            None,
        );
    }
    v.sort_by_key(|i| i.severity);
    v
}

/// (errors, warnings)
pub fn counts(issues: &[Issue]) -> (usize, usize) {
    (
        issues.iter().filter(|i| i.severity == Severity::Error).count(),
        issues.iter().filter(|i| i.severity == Severity::Warning).count(),
    )
}
