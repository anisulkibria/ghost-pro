//! The two migration directories must describe the same database.
//!
//! `migrations/sqlite` and `migrations/mysql` are maintained by hand, because
//! the dialects differ in ways that cannot be generated from one another — a
//! `TEXT` primary key, a partial index, a functional index. The cost of that
//! is drift: a column added to one file and forgotten in the other produces a
//! platform that works locally and fails in production on the query that
//! touches it.
//!
//! So the shapes are compared here. Types are deliberately *not* compared —
//! `TEXT` against `VARCHAR(255)` is the whole point of having two files. What
//! must match is the set of tables and the set of column names in each, which
//! is what every query in this crate actually depends on.
//!
//! Runs on any machine, with no database of either kind.
use std::collections::{BTreeMap, BTreeSet};

/// Table name -> its column names, as declared in one directory's SQL.
type Schema = BTreeMap<String, BTreeSet<String>>;

fn read_dir(engine: &str) -> String {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations");
    let dir = std::path::Path::new(dir).join(engine);
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    // Applied in filename order, and `ALTER TABLE` in a later file depends on
    // the `CREATE TABLE` in an earlier one.
    files.sort();
    assert!(!files.is_empty(), "no migrations in {}", dir.display());
    files.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect::<Vec<_>>().join("\n")
}

/// Strip comments and normalise whitespace, so the parse below only sees SQL.
fn strip(sql: &str) -> String {
    sql.lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pull the tables and column names out of a directory's DDL.
///
/// A deliberately small parser: these two files are the only input it will
/// ever see, and anything it cannot read it reports rather than skipping, so
/// it cannot quietly stop checking.
fn parse(sql: &str) -> Schema {
    let sql = strip(sql);
    let mut schema = Schema::new();

    // CREATE TABLE [IF NOT EXISTS] name ( ... );
    let mut rest = sql.as_str();
    while let Some(at) = rest.find("CREATE TABLE") {
        rest = &rest[at + "CREATE TABLE".len()..];
        let head = rest.trim_start().trim_start_matches("IF NOT EXISTS").trim_start();
        let open = head.find('(').expect("CREATE TABLE with no (");
        let name = head[..open].trim().trim_matches('`').to_string();

        // The matching close paren, tracking nesting so a `VARCHAR(24)` or a
        // `CONSTRAINT … (col)` does not end the body early.
        let body_start = open + 1;
        let bytes = head.as_bytes();
        let mut depth = 1usize;
        let mut i = body_start;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        let body = &head[body_start..i - 1];

        let mut cols = BTreeSet::new();
        for part in split_top_level(body) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let first = part.split_whitespace().next().unwrap_or_default();
            let upper = first.to_ascii_uppercase();
            // Table-level clauses are not columns. `KEY` and `INDEX` are
            // deliberately absent from this list: MySQL accepts them as
            // inline index clauses, but neither file uses that syntax, and
            // treating them as clauses would silently skip the real column
            // named `key` in `platform_settings`.
            if matches!(
                upper.as_str(),
                "PRIMARY" | "UNIQUE" | "FOREIGN" | "CONSTRAINT" | "CHECK"
            ) {
                continue;
            }
            cols.insert(first.trim_matches('`').to_string());
        }
        schema.insert(name, cols);
        rest = &head[i..];
    }

    // ALTER TABLE name ADD COLUMN col …
    for stmt in sql.split("ALTER TABLE").skip(1) {
        let mut words = stmt.split_whitespace();
        let table = words.next().unwrap_or_default().trim_matches('`').to_string();
        let rest: Vec<&str> = words.collect();
        if let Some(pos) = rest.iter().position(|w| w.eq_ignore_ascii_case("COLUMN")) {
            if let Some(col) = rest.get(pos + 1) {
                schema
                    .get_mut(&table)
                    .unwrap_or_else(|| panic!("ALTER on unknown table {table}"))
                    .insert(col.trim_matches('`').to_string());
            }
        }
    }

    schema
}

/// Split a `CREATE TABLE` body on commas that are not inside parentheses.
fn split_top_level(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out
}

/// The same tables, with the same columns, on both engines.
///
/// This is the test referred to by `pro_db::pool::migrate`. If it fails, one
/// of the two migration directories has a change the other does not — and the
/// query code above cannot tell, because it is written once for both.
#[test]
fn both_engines_declare_the_same_tables() {
    let sqlite = parse(&read_dir("sqlite"));
    let mysql = parse(&read_dir("mysql"));

    assert!(!sqlite.is_empty(), "parsed no tables out of migrations/sqlite");

    let sqlite_tables: BTreeSet<_> = sqlite.keys().cloned().collect();
    let mysql_tables: BTreeSet<_> = mysql.keys().cloned().collect();

    let only_sqlite: Vec<_> = sqlite_tables.difference(&mysql_tables).collect();
    let only_mysql: Vec<_> = mysql_tables.difference(&sqlite_tables).collect();
    assert!(
        only_sqlite.is_empty() && only_mysql.is_empty(),
        "the two schemas have drifted.\n  only in sqlite: {only_sqlite:?}\n  only in mysql:  {only_mysql:?}"
    );

    for (table, sqlite_cols) in &sqlite {
        let mysql_cols = &mysql[table];
        let missing: Vec<_> = sqlite_cols.difference(mysql_cols).collect();
        let extra: Vec<_> = mysql_cols.difference(sqlite_cols).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{table} differs between engines.\n  missing from mysql:  {missing:?}\n  \
             missing from sqlite: {extra:?}"
        );
    }
}

/// No column that the platform reads may be declared `TEXT` on MySQL.
///
/// MySQL sends a `TEXT` column as a blob, and sqlx's `Any` driver will not
/// decode a blob into a `String` — so such a column migrates, inserts, and
/// then fails the first time a page reads it. That is a production-only
/// failure, which is exactly the kind this test exists to make impossible.
///
/// Checked as an absolute rule rather than per column, because "is it read
/// anywhere?" is not something a schema file can answer, and a column that is
/// write-only today gets read tomorrow.
#[test]
fn the_mysql_schema_has_no_unreadable_text_columns() {
    let sql = strip(&read_dir("mysql"));
    let mut offenders = Vec::new();

    for line in sql.lines() {
        let trimmed = line.trim();
        // Column declarations only: `name TYPE …`, inside a CREATE TABLE.
        let mut words = trimmed.split_whitespace();
        let (Some(name), Some(ty)) = (words.next(), words.next()) else { continue };
        let ty = ty.trim_end_matches(',').to_ascii_uppercase();
        if matches!(ty.as_str(), "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "BLOB") {
            offenders.push(format!("{} {}", name.trim_matches('`'), ty));
        }
    }

    assert!(
        offenders.is_empty(),
        "these MySQL columns cannot be decoded into a String by sqlx's Any \
         driver — use VARCHAR(n) instead: {offenders:?}"
    );
}

/// `TINYINT` is likewise undecodable: `Any` maps only `SMALLINT`, `INT` and
/// `BIGINT` from MySQL, and `BOOLEAN` is an alias for `TINYINT(1)`.
#[test]
fn the_mysql_schema_has_no_unreadable_integer_columns() {
    let sql = strip(&read_dir("mysql"));
    let mut offenders = Vec::new();

    for line in sql.lines() {
        let trimmed = line.trim();
        let mut words = trimmed.split_whitespace();
        let (Some(name), Some(ty)) = (words.next(), words.next()) else { continue };
        let ty = ty.trim_end_matches(',').to_ascii_uppercase();
        let base = ty.split('(').next().unwrap_or(&ty);
        if matches!(base, "TINYINT" | "BOOLEAN" | "BOOL" | "MEDIUMINT" | "DECIMAL" | "NUMERIC") {
            offenders.push(format!("{} {}", name.trim_matches('`'), ty));
        }
    }

    assert!(
        offenders.is_empty(),
        "these MySQL columns cannot be decoded by sqlx's Any driver — use \
         SMALLINT, INT or BIGINT: {offenders:?}"
    );
}

/// Every `VARCHAR` in a table has to fit MySQL's 65,535-byte row limit, and
/// `utf8mb4` charges four bytes per declared character.
///
/// Exceeding it is a migration-time error, so this only turns a failed deploy
/// into a failed test — but that is the difference between finding out now and
/// finding out while rolling out.
#[test]
fn no_mysql_table_exceeds_the_row_size_limit() {
    let sqlite = parse(&read_dir("sqlite"));
    let sql = strip(&read_dir("mysql"));

    // Re-walk the DDL, summing declared VARCHAR widths per table.
    let mut current: Option<String> = None;
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    for line in sql.lines() {
        let trimmed = line.trim();
        if let Some(at) = trimmed.find("CREATE TABLE") {
            let head = trimmed[at + "CREATE TABLE".len()..].trim();
            let name = head.split('(').next().unwrap_or(head).trim().trim_matches('`');
            current = Some(name.to_string());
            continue;
        }
        if let Some(table) = &current {
            if let Some(open) = trimmed.find("VARCHAR(") {
                let after = &trimmed[open + "VARCHAR(".len()..];
                if let Some(close) = after.find(')') {
                    if let Ok(n) = after[..close].parse::<usize>() {
                        *totals.entry(table.clone()).or_default() += n * 4;
                    }
                }
            }
        }
    }

    for (table, bytes) in &totals {
        assert!(
            *bytes < 65_535,
            "{table} declares {bytes} bytes of VARCHAR in utf8mb4, over MySQL's \
             65,535-byte row limit — CREATE TABLE will fail"
        );
    }
    // And the walk has to have actually seen the tables, or it proves nothing.
    assert_eq!(
        totals.len(),
        sqlite.len(),
        "the row-size check only looked at {} of {} tables",
        totals.len(),
        sqlite.len()
    );
}
