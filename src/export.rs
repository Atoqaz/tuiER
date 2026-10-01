//! Exports of the whole schema: Mermaid ER diagram, Graphviz DOT and a
//! Markdown data dictionary. All exports include the full descriptions.

use crate::diagram::truncate;
use crate::schema::{ident_sql, needs_quotes, Schema, COLORS_TABLE, LAYOUT_TABLE};
use std::collections::HashSet;
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mermaid,
    Graphviz,
    Markdown,
    Sql,
}

/// What an export needs besides the schema.
pub struct ExportCtx<'a> {
    pub title: &'a str,
    pub desc_width: usize,
    pub desc_table: &'a str,
    /// saved table positions, included in the SQL export
    pub layout: &'a [(String, usize, usize)],
    /// chosen table colours (table, colour), included in the SQL export
    pub colors: &'a [(String, String)],
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Mermaid, Format::Graphviz, Format::Markdown, Format::Sql];

    pub fn parse(s: &str) -> Option<Format> {
        match s.to_lowercase().as_str() {
            "mermaid" | "mmd" => Some(Format::Mermaid),
            "graphviz" | "dot" => Some(Format::Graphviz),
            "markdown" | "md" => Some(Format::Markdown),
            "sql" => Some(Format::Sql),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Mermaid => "mmd",
            Format::Graphviz => "dot",
            Format::Markdown => "md",
            Format::Sql => "sql",
        }
    }

    pub fn render(self, schema: &Schema, ctx: &ExportCtx) -> String {
        match self {
            Format::Mermaid => mermaid(schema),
            Format::Graphviz => graphviz(schema, ctx.desc_width),
            Format::Markdown => markdown(schema, ctx.title),
            Format::Sql => sql(schema, ctx),
        }
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Mermaid identifiers allow letters, digits, '_' and '-'.
fn ident(s: &str) -> String {
    let mut o: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if o.is_empty() || o.starts_with(|c: char| c.is_ascii_digit()) {
        o.insert(0, '_');
    }
    o
}

pub fn mermaid(s: &Schema) -> String {
    let mut o = String::from("erDiagram\n");
    for t in &s.tables {
        let _ = writeln!(o, "    {} {{", ident(&t.name));
        for c in &t.columns {
            let ty = if c.data_type.is_empty() { "ANY".to_string() } else { ident(&c.data_type) };
            let mut keys = vec![];
            if c.is_pk() {
                keys.push("PK");
            }
            if c.fk {
                keys.push("FK");
            }
            if c.unique && !c.is_pk() {
                keys.push("UK");
            }
            let _ = write!(o, "        {ty} {}", ident(&c.name));
            if !keys.is_empty() {
                let _ = write!(o, " {}", keys.join(", "));
            }
            if let Some(d) = &c.description {
                let _ = write!(o, " \"{}\"", one_line(d).replace('"', "'"));
            }
            o.push('\n');
        }
        o.push_str("    }\n");
    }
    for fk in &s.fks {
        let from = s.col(fk.from);
        // parent (one) to child (many); optional parent when the FK is nullable
        let card = if from.not_null { "||--o{" } else { "|o--o{" };
        let _ = writeln!(
            o,
            "    {} {card} {} : \"{}\"",
            ident(&s.tables[fk.to.table].name),
            ident(&s.tables[fk.from.table].name),
            from.name.replace('"', "'")
        );
    }
    o
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn graphviz(s: &Schema, desc_width: usize) -> String {
    let mut o = String::from(
        "digraph erd {\n  rankdir=RL;\n  graph [nodesep=0.5, ranksep=1.2];\n  \
         node [shape=plaintext, fontname=\"Helvetica\", fontsize=10];\n  \
         edge [color=\"#777777\", arrowhead=normal];\n\n",
    );
    for (ti, t) in s.tables.iter().enumerate() {
        let _ = write!(
            o,
            "  t{ti} [label=<<table border=\"0\" cellborder=\"1\" cellspacing=\"0\" cellpadding=\"4\">\
             <tr><td colspan=\"4\" bgcolor=\"#dddddd\"><b>{}</b></td></tr>",
            html(&t.name)
        );
        if let Some(d) = &t.description {
            let _ = write!(o, "<tr><td colspan=\"4\" align=\"left\"><i>{}</i></td></tr>", html(&truncate(d, desc_width * 2)));
        }
        for (ci, c) in t.columns.iter().enumerate() {
            let key = match (c.is_pk(), c.fk) {
                (true, true) => "<font color=\"#b58900\">P</font><font color=\"#2aa198\">F</font>",
                (true, false) => "<font color=\"#b58900\"><b>PK</b></font>",
                (false, true) => "<font color=\"#2aa198\"><b>FK</b></font>",
                _ => "",
            };
            let desc = c.description.as_deref().map(|d| html(&truncate(d, desc_width))).unwrap_or_default();
            let _ = write!(
                o,
                "<tr><td port=\"c{ci}\">{key}</td><td align=\"left\">{}</td>\
                 <td align=\"left\"><font color=\"#666666\">{}</font></td><td align=\"left\">{desc}</td></tr>",
                html(&c.name),
                html(&c.data_type)
            );
        }
        o.push_str("</table>>];\n");
    }
    o.push('\n');
    for fk in &s.fks {
        let _ = writeln!(o, "  t{}:c{} -> t{}:c{};", fk.from.table, fk.from.column, fk.to.table, fk.to.column);
    }
    o.push_str("}\n");
    o
}

fn md(s: &str) -> String {
    one_line(s).replace('|', "\\|")
}

pub fn markdown(s: &Schema, title: &str) -> String {
    let mut o = format!("# Data dictionary: {}\n\n", md(title));
    o.push_str("| Table | Columns | Description |\n|---|---|---|\n");
    for t in &s.tables {
        let _ = writeln!(
            o,
            "| [{}](#{}) | {} | {} |",
            md(&t.name),
            t.name.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', ""),
            t.columns.len(),
            t.description.as_deref().map(md).unwrap_or_default()
        );
    }
    for (ti, t) in s.tables.iter().enumerate() {
        let _ = write!(o, "\n## {}\n\n", md(&t.name));
        if let Some(d) = &t.description {
            let _ = write!(o, "{}\n\n", d.trim());
        }
        o.push_str("| Key | Column | Type | Null | Default | References | Description |\n");
        o.push_str("|---|---|---|---|---|---|---|\n");
        for (ci, c) in t.columns.iter().enumerate() {
            let mut key = vec![];
            if c.is_pk() {
                key.push("PK");
            }
            if c.fk {
                key.push("FK");
            }
            if c.unique && !c.is_pk() {
                key.push("UQ");
            }
            let refs: Vec<String> = s
                .fks
                .iter()
                .filter(|f| f.from.table == ti && f.from.column == ci)
                .map(|f| format!("`{}`", s.qualified(f.to)))
                .collect();
            let _ = writeln!(
                o,
                "| {} | `{}` | {} | {} | {} | {} | {} |",
                key.join(", "),
                md(&c.name),
                md(&c.data_type),
                if c.not_null { "no" } else { "yes" },
                c.default.as_deref().map(|d| format!("`{}`", md(d))).unwrap_or_default(),
                refs.join(", "),
                c.description.as_deref().map(md).unwrap_or_default()
            );
        }
        let mut by: Vec<String> = s
            .fks
            .iter()
            .filter(|f| f.to.table == ti)
            .map(|f| format!("`{}`", s.qualified(f.from)))
            .collect();
        by.dedup();
        if !by.is_empty() {
            let _ = write!(o, "\nReferenced by: {}\n", by.join(", "));
        }
        if !t.indexes.is_empty() {
            o.push_str("\nIndexes:\n\n");
            for i in &t.indexes {
                let kind = match (i.origin.as_str(), i.unique) {
                    ("pk", _) => "primary key",
                    ("u", _) => "unique constraint",
                    (_, true) => "unique",
                    _ => "index",
                };
                let _ = writeln!(o, "- `{}` ({kind}): {}", md(&i.name), md(&i.columns.join(", ")));
            }
        }
    }
    o
}

fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn statement(sql: &str, names: &HashSet<String>) -> String {
    let t = unquote_identifiers(sql.trim_end(), names);
    if t.ends_with(';') { t } else { format!("{t};") }
}

/// Every name the schema defines (lowercase): tables, columns, indexes, views, triggers.
fn known_names(s: &Schema) -> HashSet<String> {
    let mut n = HashSet::new();
    for t in &s.tables {
        n.insert(t.name.to_lowercase());
        n.extend(t.columns.iter().map(|c| c.name.to_lowercase()));
        n.extend(t.indexes.iter().map(|i| i.name.to_lowercase()));
    }
    n.extend(s.others.iter().map(|o| o.1.to_lowercase()));
    n
}

/// Drop the double quotes around identifiers that don't need them.
/// Only names the schema defines are touched: SQLite reads an unknown
/// "word" as a string, so unquoting it could change the meaning.
/// Strings, comments and other quoting styles are copied unchanged.
pub fn unquote_identifiers(sql: &str, names: &HashSet<String>) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    // copy a quoted run; a doubled quote char is an escaped one
    let quoted_end = |start: usize, q: char| -> usize {
        let mut j = start + 1;
        while j < chars.len() {
            if chars[j] == q {
                if chars.get(j + 1) == Some(&q) {
                    j += 2;
                    continue;
                }
                return j + 1;
            }
            j += 1;
        }
        chars.len()
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            let e = quoted_end(i, '\'');
            out.extend(&chars[i..e]);
            i = e;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            let e = chars[i..].iter().position(|&x| x == '\n').map(|p| i + p).unwrap_or(chars.len());
            out.extend(&chars[i..e]);
            i = e;
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            let e = (i + 2..chars.len().saturating_sub(1))
                .find(|&j| chars[j] == '*' && chars[j + 1] == '/')
                .map(|j| j + 2)
                .unwrap_or(chars.len());
            out.extend(&chars[i..e]);
            i = e;
        } else if c == '"' {
            let e = quoted_end(i, '"');
            let inner: String = chars[i + 1..e.saturating_sub(1).max(i + 1)].iter().collect::<String>().replace("\"\"", "\"");
            if e > i + 1 && chars[e - 1] == '"' && !needs_quotes(&inner) && names.contains(&inner.to_lowercase()) {
                out.push_str(&inner);
            } else {
                out.extend(&chars[i..e]);
            }
            i = e;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Tables ordered so referenced tables come before the tables pointing at them.
fn creation_order(s: &Schema) -> Vec<usize> {
    let n = s.tables.len();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut by_name: Vec<usize> = (0..n).collect();
    by_name.sort_by_key(|&t| s.tables[t].name.to_lowercase());
    while order.len() < n {
        let ready = by_name.iter().copied().find(|&t| {
            !done[t] && s.fks.iter().all(|f| f.from.table != t || f.to.table == t || done[f.to.table])
        });
        // a cycle of references: take the next one anyway (foreign keys are off while loading)
        let t = ready.unwrap_or_else(|| by_name.iter().copied().find(|&t| !done[t]).unwrap());
        done[t] = true;
        order.push(t);
    }
    order
}

/// A script that recreates the whole database structure: tables, indexes,
/// views, triggers, plus the descriptions and saved layout. No row data.
pub fn sql(s: &Schema, ctx: &ExportCtx) -> String {
    let names = known_names(s);
    let mut o = format!(
        "-- Schema of {}\n-- Generated by tuiER. Recreate with:  sqlite3 new.db < this-file.sql\n\n\
         PRAGMA foreign_keys = OFF;\nBEGIN TRANSACTION;\n",
        ctx.title
    );
    for t in creation_order(s) {
        let table = &s.tables[t];
        let _ = writeln!(o, "\n-- {}", table.name);
        if let Some(d) = &table.description {
            let _ = writeln!(o, "-- {}", one_line(d));
        }
        match &table.sql {
            Some(sql) => o.push_str(&statement(sql, &names)),
            None => {
                let _ = write!(o, "-- (no CREATE statement available for {})", table.name);
            }
        }
        o.push('\n');
        for i in table.indexes.iter().rev() {
            // automatic indexes (PRIMARY KEY / UNIQUE) come with the table
            if let Some(sql) = &i.sql {
                o.push_str(&statement(sql, &names));
                o.push('\n');
            }
        }
    }
    for (kind, name, sql) in &s.others {
        let _ = writeln!(o, "\n-- {kind} {name}\n{}", statement(sql, &names));
    }

    let mut descs: Vec<(String, Option<String>, String)> = Vec::new();
    for t in &s.tables {
        if let Some(d) = &t.description {
            descs.push((t.name.clone(), None, d.clone()));
        }
        for c in &t.columns {
            if let Some(d) = &c.description {
                descs.push((t.name.clone(), Some(c.name.clone()), d.clone()));
            }
        }
    }
    if !descs.is_empty() {
        let q = ident_sql(ctx.desc_table);
        let _ = writeln!(
            o,
            "\n-- Descriptions (used by tuiER)\nCREATE TABLE IF NOT EXISTS {q} (\n    table_name  TEXT NOT NULL,\n    column_name TEXT,\n    description TEXT NOT NULL\n);"
        );
        for (t, c, d) in descs {
            let c = c.as_deref().map(sql_str).unwrap_or_else(|| "NULL".into());
            let _ = writeln!(o, "INSERT INTO {q} (table_name, column_name, description) VALUES ({}, {c}, {});", sql_str(&t), sql_str(&d));
        }
    }
    if !ctx.layout.is_empty() {
        let q = ident_sql(LAYOUT_TABLE);
        let _ = writeln!(
            o,
            "\n-- Diagram layout (used by tuiER)\nCREATE TABLE IF NOT EXISTS {q} (\n    table_name TEXT PRIMARY KEY,\n    grid_col   INTEGER NOT NULL,\n    grid_row   INTEGER NOT NULL\n);"
        );
        for (t, c, r) in ctx.layout {
            let _ = writeln!(o, "INSERT INTO {q} (table_name, grid_col, grid_row) VALUES ({}, {c}, {r});", sql_str(t));
        }
    }
    if !ctx.colors.is_empty() {
        let q = ident_sql(COLORS_TABLE);
        let _ = writeln!(o, "\n-- Table colours (used by tuiER)\nCREATE TABLE IF NOT EXISTS {q} (\n    table_name TEXT PRIMARY KEY,\n    color      TEXT NOT NULL\n);");
        for (t, c) in ctx.colors {
            let _ = writeln!(o, "INSERT INTO {q} (table_name, color) VALUES ({}, {});", sql_str(t), sql_str(c));
        }
    }
    o.push_str("\nCOMMIT;\nPRAGMA foreign_keys = ON;\n");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_only_known_plain_identifiers() {
        let names: HashSet<String> = ["users", "id", "order", "first name", "org_id"].iter().map(|s| s.to_string()).collect();
        let sql = r#"CREATE TABLE "users" ("id" INTEGER, "order" TEXT, "first name" TEXT, "org_id" INT DEFAULT 'a "id" b', x TEXT CHECK (x <> "unknown")) -- "id""#;
        let out = unquote_identifiers(sql, &names);
        assert_eq!(
            out,
            r#"CREATE TABLE users (id INTEGER, "order" TEXT, "first name" TEXT, org_id INT DEFAULT 'a "id" b', x TEXT CHECK (x <> "unknown")) -- "id""#
        );
    }
}
