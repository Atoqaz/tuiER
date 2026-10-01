//! Small helpers for reading SQL text: SQLite doesn't expose CHECK
//! constraints, COLLATE, AUTOINCREMENT or table-level UNIQUE through
//! pragmas, so they are read from the CREATE TABLE statement itself.

use std::collections::HashMap;

/// Where a quoted run starting at `start` (with quote char `q`) ends.
/// A doubled quote char is an escaped one; `[` closes with `]`.
fn quoted_end(chars: &[char], start: usize) -> usize {
    let q = if chars[start] == '[' { ']' } else { chars[start] };
    let mut j = start + 1;
    while j < chars.len() {
        if chars[j] == q {
            if q != ']' && chars.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    chars.len()
}

fn is_quote(c: char) -> bool {
    matches!(c, '\'' | '"' | '`' | '[')
}

/// Contents of the parentheses opening at or after `from` (skipping
/// whitespace only), and the index just past the closing one.
fn balanced_at(chars: &[char], from: usize) -> Option<(String, usize)> {
    let mut i = from;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if chars.get(i) != Some(&'(') {
        return None;
    }
    let start = i + 1;
    let mut depth = 0;
    while i < chars.len() {
        match chars[i] {
            c if is_quote(c) => {
                i = quoted_end(chars, i);
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((chars[start..i].iter().collect::<String>().trim().to_string(), i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split on commas that are not inside parentheses or quotes.
pub fn split_top_level(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let (mut depth, mut start, mut i) = (0i32, 0usize, 0usize);
    while i < chars.len() {
        match chars[i] {
            c if is_quote(c) => {
                i = quoted_end(&chars, i);
                continue;
            }
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(chars[start..i].iter().collect::<String>());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(chars[start..].iter().collect());
    parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

/// A word or quoted name with its position (char indices).
struct Tok {
    text: String,
    upper: String,
    end: usize,
}

fn tokens(chars: &[char]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphanumeric() || c == '_' {
            let s = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let text: String = chars[s..i].iter().collect();
            out.push(Tok { upper: text.to_uppercase(), text, end: i });
        } else if is_quote(c) {
            let e = quoted_end(chars, i);
            let text: String = chars[i..e].iter().collect();
            out.push(Tok { upper: String::new(), text, end: e });
            i = e;
        } else {
            i += 1;
        }
    }
    out
}

/// "name", `name`, [name] or 'name' → name
pub fn unquote(s: &str) -> String {
    let s = s.trim();
    let mut c = s.chars();
    match (c.next(), s.chars().last()) {
        (Some(a @ ('"' | '`' | '\'')), Some(b)) if a == b && s.len() >= 2 => {
            s[1..s.len() - 1].replace(&format!("{a}{a}"), &a.to_string())
        }
        (Some('['), Some(']')) if s.len() >= 2 => s[1..s.len() - 1].to_string(),
        _ => s.to_string(),
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct ColumnExtras {
    pub collate: Option<String>,
    pub checks: Vec<String>,
    pub autoincrement: bool,
}

#[derive(Default, Debug)]
pub struct ParsedTable {
    /// by lowercase column name
    pub columns: HashMap<String, ColumnExtras>,
    pub table_checks: Vec<String>,
    /// table-level UNIQUE (a, b): the text inside the parentheses
    pub table_uniques: Vec<String>,
    /// CHECK / COLLATE keywords seen vs understood; a mismatch means
    /// something wasn't read and would be lost on a rebuild
    pub unread: Vec<String>,
}

/// Read what pragmas don't tell from a CREATE TABLE statement.
pub fn parse_create_table(sql: &str) -> ParsedTable {
    let mut p = ParsedTable::default();
    let chars: Vec<char> = sql.chars().collect();
    let Some(open) = chars.iter().position(|&c| c == '(') else { return p };
    let Some((body, _)) = balanced_at(&chars, open) else { return p };

    let (mut checks_read, mut collates_read) = (0, 0);
    for part in split_top_level(&body) {
        let pc: Vec<char> = part.chars().collect();
        let toks = tokens(&pc);
        let mut k = 0;
        if toks.first().is_some_and(|t| t.upper == "CONSTRAINT") {
            k = 2; // CONSTRAINT name
        }
        let Some(first) = toks.get(k) else { continue };
        match first.upper.as_str() {
            "CHECK" => {
                if let Some((e, _)) = balanced_at(&pc, first.end) {
                    p.table_checks.push(e);
                    checks_read += 1;
                }
            }
            "UNIQUE" => {
                if let Some((cols, _)) = balanced_at(&pc, first.end) {
                    p.table_uniques.push(cols);
                }
            }
            "PRIMARY" | "FOREIGN" => {}
            _ => {
                let name = unquote(&first.text).to_lowercase();
                let mut ex = ColumnExtras::default();
                let mut j = k + 1;
                while j < toks.len() {
                    match toks[j].upper.as_str() {
                        "CHECK" => {
                            if let Some((e, end)) = balanced_at(&pc, toks[j].end) {
                                ex.checks.push(e);
                                checks_read += 1;
                                // skip tokens inside the parentheses
                                while j + 1 < toks.len() && toks[j + 1].end <= end {
                                    j += 1;
                                }
                            }
                        }
                        "COLLATE" => {
                            if let Some(t) = toks.get(j + 1) {
                                ex.collate = Some(unquote(&t.text));
                                collates_read += 1;
                                j += 1;
                            }
                        }
                        "AUTOINCREMENT" => ex.autoincrement = true,
                        "DEFAULT" => {
                            // DEFAULT (expr) may contain anything: skip it
                            if let Some((_, end)) = balanced_at(&pc, toks[j].end) {
                                while j + 1 < toks.len() && toks[j + 1].end <= end {
                                    j += 1;
                                }
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                p.columns.insert(name, ex);
            }
        }
    }
    let words: Vec<String> = tokens(&chars).into_iter().map(|t| t.upper).collect();
    let count = |w: &str| words.iter().filter(|x| *x == w).count();
    if count("CHECK") != checks_read {
        p.unread.push("CHECK constraints the editor couldn't read".into());
    }
    if count("COLLATE") != collates_read {
        p.unread.push("COLLATE clauses the editor couldn't read".into());
    }
    p
}

/// Column list and WHERE clause of a CREATE INDEX statement.
pub fn parse_index_sql(sql: &str) -> Option<(String, String)> {
    let chars: Vec<char> = sql.chars().collect();
    let toks = tokens(&chars);
    let on = toks.iter().position(|t| t.upper == "ON")?;
    let table = toks.get(on + 1)?;
    let (cols, end) = balanced_at(&chars, table.end)?;
    let rest: String = chars[end..].iter().collect::<String>().trim().trim_end_matches(';').trim().to_string();
    let where_ = rest.strip_prefix("WHERE").or_else(|| rest.strip_prefix("where")).unwrap_or("").trim().to_string();
    Some((cols, where_))
}

/// Replace identifiers (outside strings) by their new names, all at once,
/// so swaps work. `map` is keyed by lowercase old name.
pub fn rename_idents(text: &str, map: &HashMap<String, String>) -> String {
    if map.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            let e = quoted_end(&chars, i);
            out.extend(&chars[i..e]);
            i = e;
        } else if matches!(c, '"' | '`' | '[') {
            let e = quoted_end(&chars, i);
            let raw: String = chars[i..e].iter().collect();
            match map.get(&unquote(&raw).to_lowercase()) {
                Some(new) => out.push_str(&crate::schema::ident_sql(new)),
                None => out.push_str(&raw),
            }
            i = e;
        } else if c.is_alphabetic() || c == '_' {
            let s = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let w: String = chars[s..i].iter().collect();
            match map.get(&w.to_lowercase()) {
                Some(new) => out.push_str(&crate::schema::ident_sql(new)),
                None => out.push_str(&w),
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_checks_collations_and_uniques() {
        let p = parse_create_table(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             name TEXT COLLATE NOCASE NOT NULL CHECK (length(name) > 0), \
             qty INT DEFAULT (1 + 1) CHECK (qty >= 0 AND qty < 10), \"odd, name\" TEXT, \
             CONSTRAINT pos CHECK (qty <> 3), UNIQUE (name, qty), FOREIGN KEY (qty) REFERENCES x(y))",
        );
        assert!(p.columns["id"].autoincrement);
        assert_eq!(p.columns["name"].collate.as_deref(), Some("NOCASE"));
        assert_eq!(p.columns["name"].checks, ["length(name) > 0"]);
        assert_eq!(p.columns["qty"].checks, ["qty >= 0 AND qty < 10"]);
        assert!(p.columns.contains_key("odd, name"));
        assert_eq!(p.table_checks, ["qty <> 3"]);
        assert_eq!(p.table_uniques, ["name, qty"]);
        assert!(p.unread.is_empty(), "{:?}", p.unread);
    }

    #[test]
    fn index_sql_and_renaming() {
        assert_eq!(
            parse_index_sql("CREATE UNIQUE INDEX ix ON \"t\" (a COLLATE NOCASE, b DESC) WHERE b > 0").unwrap(),
            ("a COLLATE NOCASE, b DESC".to_string(), "b > 0".to_string())
        );
        let map: HashMap<String, String> = [("a".to_string(), "b".to_string()), ("b".to_string(), "a".to_string())].into();
        assert_eq!(rename_idents("a > b AND \"a\" <> 'a'", &map), "b > a AND b <> 'a'");
    }
}
