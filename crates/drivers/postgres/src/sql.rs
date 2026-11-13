//! SQL text helpers: identifier/literal quoting, filter and paging SQL generation, and a small
//! lexer used to split scripts into statements.

use dbstudio_core::{ColumnFilter, Error, FilterOp, ObjectPath, Result, SortKey};

/// Quote an identifier (always quoted, embedded quotes doubled).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a `schema.name` pair.
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Render `value` as an SQL string literal that is safe regardless of the
/// `standard_conforming_strings` setting: quotes are doubled and, when a backslash is present, the
/// `E''` form with doubled backslashes is used.
pub fn quote_literal(value: &str) -> String {
    let quoted = value.replace('\'', "''");
    if value.contains('\\') {
        format!("E'{}'", quoted.replace('\\', "\\\\"))
    } else {
        format!("'{quoted}'")
    }
}

/// Escape the `LIKE` wildcards (`%`, `_`) and the escape character itself.
pub fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// NUL bytes cannot be sent in a simple-protocol statement (they would truncate it).
pub fn check_no_nul(text: &str, what: &str) -> Result<()> {
    if text.contains('\0') {
        Err(Error::query(format!(
            "{what} must not contain a NUL character"
        )))
    } else {
        Ok(())
    }
}

fn first_value(f: &ColumnFilter) -> Result<&str> {
    f.values.first().map(String::as_str).ok_or_else(|| {
        Error::query(format!(
            "filter on column \"{}\" needs a value",
            f.column.replace('"', "'")
        ))
    })
}

/// SQL condition of one filter.
pub fn filter_condition(f: &ColumnFilter) -> Result<String> {
    check_no_nul(&f.column, "column name")?;
    for v in &f.values {
        check_no_nul(v, "filter value")?;
    }
    let col = quote_ident(&f.column);
    let cmp = |sym: &str| -> Result<String> {
        Ok(format!("{col} {sym} {}", quote_literal(first_value(f)?)))
    };
    let like = |negated: bool, prefix: &str, suffix: &str| -> Result<String> {
        let pattern = format!("{prefix}{}{suffix}", escape_like(first_value(f)?));
        Ok(format!(
            "{col}::text {}ILIKE {} ESCAPE E'\\\\'",
            if negated { "NOT " } else { "" },
            quote_literal(&pattern)
        ))
    };
    match f.op {
        FilterOp::Eq => cmp("="),
        FilterOp::Ne => cmp("<>"),
        FilterOp::Lt => cmp("<"),
        FilterOp::Le => cmp("<="),
        FilterOp::Gt => cmp(">"),
        FilterOp::Ge => cmp(">="),
        FilterOp::Contains => like(false, "%", "%"),
        FilterOp::NotContains => like(true, "%", "%"),
        FilterOp::StartsWith => like(false, "", "%"),
        FilterOp::EndsWith => like(false, "%", ""),
        FilterOp::IsNull => Ok(format!("{col} IS NULL")),
        FilterOp::IsNotNull => Ok(format!("{col} IS NOT NULL")),
        FilterOp::In => {
            if f.values.is_empty() {
                Ok("FALSE".to_string())
            } else {
                let list: Vec<String> = f.values.iter().map(|v| quote_literal(v)).collect();
                Ok(format!("{col} IN ({})", list.join(", ")))
            }
        }
    }
}

/// `WHERE ...` clause (with leading space) combining filters and the raw clause; empty string
/// when there is nothing to filter on.
pub fn where_clause(filters: &[ColumnFilter], raw_where: Option<&str>) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for f in filters {
        parts.push(filter_condition(f)?);
    }
    if let Some(raw) = raw_where.map(str::trim).filter(|r| !r.is_empty()) {
        check_no_nul(raw, "WHERE clause")?;
        // Newlines keep a trailing `-- comment` in the raw text from swallowing the parenthesis.
        parts.push(format!("(\n{raw}\n)"));
    }
    if parts.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("\nWHERE {}", parts.join("\n  AND ")))
    }
}

/// `ORDER BY ...` clause (leading space) or an empty string. `tiebreak` columns (a key of the
/// table) are appended after the user's sort keys, skipping columns already sorted on, so that
/// `OFFSET` paging is deterministic.
pub fn order_clause(sort: &[SortKey], tiebreak: &[String]) -> Result<String> {
    let mut parts = Vec::new();
    for t in tiebreak {
        if !sort.iter().any(|s| &s.column == t) {
            check_no_nul(t, "column name")?;
            parts.push(format!("{} ASC", quote_ident(t)));
        }
    }
    if sort.is_empty() && parts.is_empty() {
        return Ok(String::new());
    }
    let tail = std::mem::take(&mut parts);
    for s in sort {
        check_no_nul(&s.column, "column name")?;
        parts.push(format!(
            "{}{}",
            quote_ident(&s.column),
            if s.descending { " DESC" } else { " ASC" }
        ));
    }
    parts.extend(tail);
    Ok(format!("\nORDER BY {}", parts.join(", ")))
}

/// The `SELECT` that implements a table page request.
pub fn select_sql(
    table: &str,
    filters: &[ColumnFilter],
    raw_where: Option<&str>,
    sort: &[SortKey],
    tiebreak: &[String],
    offset: u64,
    limit: u64,
) -> Result<String> {
    Ok(format!(
        "SELECT * FROM {table}{}{}\nLIMIT {limit} OFFSET {offset}",
        where_clause(filters, raw_where)?,
        order_clause(sort, tiebreak)?
    ))
}

/// The `SELECT count(*)` of a count request.
pub fn count_sql(table: &str, filters: &[ColumnFilter], raw_where: Option<&str>) -> Result<String> {
    Ok(format!(
        "SELECT count(*) FROM {table}{}",
        where_clause(filters, raw_where)?
    ))
}

// ---------------------------------------------------------------------------------------------
// Lexing
// ---------------------------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Copy of `sql` (same byte length) where string literals, quoted identifiers and dollar-quoted
/// bodies are replaced by `_` and comments by spaces, so that structural scanning (`;`, keywords)
/// cannot be fooled by their contents.
pub fn mask(sql: &str) -> String {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = b.to_vec();
    let mut i = 0;
    let blank = |out: &mut [u8], from: usize, to: usize, with: u8| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = with;
            }
        }
    };
    while i < n {
        match b[i] {
            b'\'' => {
                let escape =
                    i > 0 && matches!(b[i - 1], b'e' | b'E') && (i < 2 || !is_ident_byte(b[i - 2]));
                let start = i;
                i += 1;
                while i < n {
                    if escape && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'\'' {
                        if i + 1 < n && b[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let end = i.min(n);
                blank(&mut out, start, end, b'_');
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < n {
                    if b[i] == b'"' {
                        if i + 1 < n && b[i + 1] == b'"' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let end = i.min(n);
                blank(&mut out, start, end, b'_');
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                let start = i;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i, b' ');
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                let start = i;
                let mut depth = 0usize;
                while i < n {
                    if i + 1 < n && b[i] == b'/' && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < n && b[i] == b'*' && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                blank(&mut out, start, i.min(n), b' ');
            }
            b'$' if i == 0 || !is_ident_byte(b[i - 1]) => {
                let mut j = i + 1;
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                    j += 1;
                }
                let tag_ok = j < n && b[j] == b'$' && !(j > i + 1 && b[i + 1].is_ascii_digit());
                if tag_ok {
                    let delim = &sql[i..=j];
                    let end = match sql[j + 1..].find(delim) {
                        Some(p) => j + 1 + p + delim.len(),
                        None => n,
                    };
                    blank(&mut out, i, end, b'_');
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One statement of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    /// Statement text (without the terminating `;`, leading whitespace preserved).
    pub text: String,
    /// Character offset of `text` within the whole script.
    pub char_offset: usize,
}

/// Split a script at top-level semicolons, dropping statements that contain nothing but
/// whitespace and comments. Superseded by `dbstudio_sql::split` in production code; kept as
/// the reference of the differential test.
#[cfg(test)]
pub fn split_statements(sql: &str) -> Vec<Stmt> {
    let masked = mask(sql);
    let mut out = Vec::new();
    let mut start = 0;
    let mut push = |from: usize, to: usize| {
        if masked[from..to].trim().is_empty() {
            return;
        }
        out.push(Stmt {
            text: sql[from..to].to_string(),
            char_offset: sql[..from].chars().count(),
        });
    };
    for (i, c) in masked.char_indices() {
        if c == ';' {
            if open_atomic_body(&masked[start..i]) {
                continue;
            }
            push(start, i);
            start = i + 1;
        }
    }
    push(start, sql.len());
    out
}

/// True while `stmt` (masked text up to a `;`) is a `CREATE FUNCTION/PROCEDURE ... BEGIN ATOMIC`
/// body that has not reached its closing `END` yet (PostgreSQL 14+).
#[cfg(test)]
fn open_atomic_body(stmt: &str) -> bool {
    let w = words(stmt);
    let mut it = w.iter().map(String::as_str);
    if it.next() != Some("CREATE") {
        return false;
    }
    let head: Vec<&str> = w.iter().take(6).map(String::as_str).collect();
    if !head.iter().any(|x| matches!(*x, "FUNCTION" | "PROCEDURE")) {
        return false;
    }
    let Some(atomic) = w
        .windows(2)
        .position(|p| p[0] == "BEGIN" && p[1] == "ATOMIC")
    else {
        return false;
    };
    let mut depth = 0i32;
    for x in &w[atomic..] {
        match x.as_str() {
            "BEGIN" | "CASE" => depth += 1,
            "END" => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// Upper-cased words (letters, digits, `_`) of a statement, outside literals and comments.
pub fn words(sql: &str) -> Vec<String> {
    mask(sql)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_uppercase())
        .collect()
}

/// Whether `DECLARE ... CURSOR FOR <stmt>` can host the statement.
pub fn cursor_eligible(stmt: &str) -> bool {
    let w = words(stmt);
    match w.first().map(String::as_str) {
        Some("SELECT" | "VALUES" | "TABLE") => true,
        Some("WITH") => !w
            .iter()
            .any(|x| matches!(x.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE")),
        _ => false,
    }
}

/// Statements whose completion tag carries an affected-row count.
pub fn is_dml(stmt: &str) -> bool {
    matches!(
        words(stmt).first().map(String::as_str),
        Some(
            "INSERT"
                | "UPDATE"
                | "DELETE"
                | "MERGE"
                | "COPY"
                | "WITH"
                | "SELECT"
                | "FETCH"
                | "MOVE"
        )
    )
}

/// Effect of a successfully executed statement on the "inside an explicit transaction" flag.
pub fn txn_effect(stmt: &str, in_txn: bool) -> bool {
    let w = words(stmt);
    let first = w.first().map(String::as_str);
    let second = w.get(1).map(String::as_str);
    match (first, second) {
        (Some("BEGIN"), _) | (Some("START"), Some("TRANSACTION")) => true,
        (Some("COMMIT" | "END" | "ABORT"), Some("PREPARED")) => in_txn,
        (Some("ROLLBACK"), Some("PREPARED" | "TO")) => in_txn,
        (Some("COMMIT" | "END" | "ROLLBACK" | "ABORT"), _) => false,
        (Some("PREPARE"), Some("TRANSACTION")) => false,
        _ => in_txn,
    }
}

/// Last non-folder, non-root segments of a path, used by browse code.
pub fn non_folder_segments(path: &ObjectPath) -> Vec<(dbstudio_core::ObjectKind, &str)> {
    path.segments
        .iter()
        .filter(|s| s.kind != dbstudio_core::ObjectKind::Folder)
        .map(|s| (s.kind, s.name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(column: &str, op: FilterOp, values: &[&str]) -> ColumnFilter {
        ColumnFilter {
            column: column.into(),
            op,
            values: values.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn quoting_identifiers() {
        assert_eq!(quote_ident("abc"), "\"abc\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident("Has Space"), "\"Has Space\"");
        assert_eq!(qualified("s", "t\"x"), "\"s\".\"t\"\"x\"");
    }

    #[test]
    fn quoting_literals() {
        assert_eq!(quote_literal("abc"), "'abc'");
        assert_eq!(quote_literal("o'neil"), "'o''neil'");
        assert_eq!(
            quote_literal("'; DROP TABLE x; --"),
            "'''; DROP TABLE x; --'"
        );
        assert_eq!(quote_literal("a\\b"), "E'a\\\\b'");
        assert_eq!(quote_literal("\\'"), "E'\\\\'''");
    }

    #[test]
    fn like_escape() {
        assert_eq!(escape_like("50%_off\\"), "50\\%\\_off\\\\");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[test]
    fn comparison_filters() {
        assert_eq!(
            filter_condition(&f("id", FilterOp::Eq, &["5"])).unwrap(),
            "\"id\" = '5'"
        );
        assert_eq!(
            filter_condition(&f("we\"ird", FilterOp::Ge, &["x'y"])).unwrap(),
            "\"we\"\"ird\" >= 'x''y'"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::Ne, &["1"])).unwrap(),
            "\"a\" <> '1'"
        );
        assert!(filter_condition(&f("a", FilterOp::Eq, &[])).is_err());
    }

    #[test]
    fn attack_strings_stay_inside_literals() {
        let sql = filter_condition(&f("t", FilterOp::Eq, &["'; DROP TABLE x; --"])).unwrap();
        assert_eq!(sql, "\"t\" = '''; DROP TABLE x; --'");
        // The masked statement has no top-level semicolon: everything is inside the literal.
        assert!(!mask(&sql).contains(';'));
        let sql = filter_condition(&f("t", FilterOp::Eq, &["\\'; DROP TABLE x; --"])).unwrap();
        assert!(!mask(&sql).contains(';'));
        let sql =
            filter_condition(&f("t", FilterOp::Contains, &["\\'; DROP TABLE x; --"])).unwrap();
        assert!(!mask(&sql).contains(';'));
        let sql = filter_condition(&f("t", FilterOp::In, &["a", "b'); DROP TABLE x; --"])).unwrap();
        assert!(!mask(&sql).contains(';'));
        let sql = filter_condition(&f("x\"; DROP TABLE y; --", FilterOp::IsNull, &[])).unwrap();
        assert!(!mask(&sql).contains(';'));
    }

    #[test]
    fn like_filters() {
        assert_eq!(
            filter_condition(&f("n", FilterOp::Contains, &["50%_"])).unwrap(),
            "\"n\"::text ILIKE E'%50\\\\%\\\\_%' ESCAPE E'\\\\'"
        );
        assert!(
            filter_condition(&f("n", FilterOp::StartsWith, &["ab"]))
                .unwrap()
                .contains("ILIKE 'ab%'")
        );
        assert!(
            filter_condition(&f("n", FilterOp::EndsWith, &["ab"]))
                .unwrap()
                .contains("ILIKE '%ab'")
        );
        assert!(
            filter_condition(&f("n", FilterOp::NotContains, &["ab"]))
                .unwrap()
                .contains("NOT ILIKE '%ab%'")
        );
    }

    #[test]
    fn null_and_in_filters() {
        assert_eq!(
            filter_condition(&f("a", FilterOp::IsNull, &["ignored"])).unwrap(),
            "\"a\" IS NULL"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::IsNotNull, &[])).unwrap(),
            "\"a\" IS NOT NULL"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::In, &["1", "2", "x'"])).unwrap(),
            "\"a\" IN ('1', '2', 'x''')"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::In, &[])).unwrap(),
            "FALSE"
        );
    }

    #[test]
    fn nul_is_rejected() {
        assert!(filter_condition(&f("a", FilterOp::Eq, &["x\0y"])).is_err());
        assert!(filter_condition(&f("a\0", FilterOp::IsNull, &[])).is_err());
        assert!(where_clause(&[], Some("a = 1\0")).is_err());
    }

    #[test]
    fn full_select() {
        let sql = select_sql(
            &qualified("public", "t"),
            &[f("a", FilterOp::Eq, &["1"]), f("b", FilterOp::IsNull, &[])],
            Some("c > 5 -- note"),
            &[
                SortKey {
                    column: "a".into(),
                    descending: true,
                },
                SortKey {
                    column: "b".into(),
                    descending: false,
                },
            ],
            &["id".to_string(), "b".to_string()],
            20,
            10,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM \"public\".\"t\"\nWHERE \"a\" = '1'\n  AND \"b\" IS NULL\n  AND (\nc > 5 -- note\n)\nORDER BY \"a\" DESC, \"b\" ASC, \"id\" ASC\nLIMIT 10 OFFSET 20"
        );
        let plain = select_sql("\"t\"", &[], Some("  "), &[], &[], 0, 5).unwrap();
        assert_eq!(plain, "SELECT * FROM \"t\"\nLIMIT 5 OFFSET 0");
        let keyed = select_sql("\"t\"", &[], None, &[], &["id".to_string()], 0, 5).unwrap();
        assert_eq!(
            keyed,
            "SELECT * FROM \"t\"\nORDER BY \"id\" ASC\nLIMIT 5 OFFSET 0"
        );
        let count = count_sql("\"t\"", &[f("a", FilterOp::Lt, &["3"])], None).unwrap();
        assert_eq!(count, "SELECT count(*) FROM \"t\"\nWHERE \"a\" < '3'");
    }

    #[test]
    fn splitting_statements() {
        let s = split_statements("select 1; select ';' ; -- c;\n select \"a;b\"; /* x; */ ;  ");
        let texts: Vec<_> = s.iter().map(|x| x.text.trim().to_string()).collect();
        assert_eq!(texts, ["select 1", "select ';'", "-- c;\n select \"a;b\""]);
        let s = split_statements("do $$ begin perform 1; end $$; select $a$;$a$; select 2");
        assert_eq!(s.len(), 3);
        assert_eq!(s[2].text.trim(), "select 2");
        let s = split_statements("select E'it\\'s;'; select 'it''s;'");
        assert_eq!(s.len(), 2);
        let s = split_statements("select 1;\nselect 'é'; select 3");
        assert_eq!(s[1].char_offset, 9);
        assert_eq!(s[2].char_offset, "select 1;\nselect 'é';".chars().count());
        assert!(split_statements("  -- nothing\n /* at all */ ; ").is_empty());
        assert_eq!(split_statements("select $1").len(), 1);
        let s = split_statements(
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN a > 1 THEN 1 ELSE 2 END; SELECT a; END; SELECT 2; \
             CREATE PROCEDURE p() BEGIN ATOMIC SELECT 1; END;",
        );
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s[0].text.trim_end().ends_with("END"));
        assert_eq!(s[1].text.trim(), "SELECT 2");
        assert_eq!(
            split_statements("select 1 /* ; /* nested ; */ ; */ ; select 2").len(),
            2
        );
    }

    #[test]
    fn keyword_helpers() {
        assert!(cursor_eligible("  -- hi\n select 1"));
        assert!(cursor_eligible("VALUES (1)"));
        assert!(cursor_eligible("with a as (select 1) select * from a"));
        assert!(!cursor_eligible(
            "with a as (delete from t returning *) select * from a"
        ));
        assert!(cursor_eligible(
            "with a as (select 'delete') select * from a"
        ));
        assert!(!cursor_eligible("explain select 1"));
        assert!(!cursor_eligible("insert into t values (1) returning *"));
        assert!(is_dml("update t set a=1"));
        assert!(!is_dml("create table t()"));
    }

    #[test]
    fn transaction_tracking() {
        assert!(txn_effect("BEGIN", false));
        assert!(txn_effect(
            "start transaction isolation level serializable",
            false
        ));
        assert!(!txn_effect("commit", true));
        assert!(!txn_effect("ROLLBACK", true));
        assert!(txn_effect("rollback to savepoint a", true));
        assert!(!txn_effect("commit prepared 'x'", false));
        assert!(txn_effect("select 1", true));
        assert!(!txn_effect("select 1", false));
    }
}
