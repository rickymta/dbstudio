//! SQL text builders: identifier quoting, literal escaping and the `SELECT` statements used to
//! browse table data. Everything here is pure (no I/O) so it can be unit tested without a server.

use dbstudio_core::{
    ColumnFilter, Error, FilterOp, ObjectKind, ObjectPath, Result, SortKey, TableCountRequest,
    TableDataRequest,
};

/// Quote an identifier with backticks, doubling embedded backticks.
pub fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('`');
    for c in name.chars() {
        if c == '`' {
            out.push('`');
        }
        out.push(c);
    }
    out.push('`');
    out
}

/// Quote a (possibly) database-qualified table name.
pub fn quote_table(db: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(db), quote_ident(table))
}

/// Render `s` as a single-quoted string literal.
///
/// When `no_backslash_escapes` is false (the default `sql_mode`) backslashes and control
/// characters are escaped with backslash sequences; when true (`NO_BACKSLASH_ESCAPES`) only the
/// quote character is doubled, because a backslash is an ordinary character in that mode.
pub fn quote_literal(s: &str, no_backslash_escapes: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if !no_backslash_escapes => out.push_str("\\\\"),
            '\0' if !no_backslash_escapes => out.push_str("\\0"),
            '\n' if !no_backslash_escapes => out.push_str("\\n"),
            '\r' if !no_backslash_escapes => out.push_str("\\r"),
            '\x1a' if !no_backslash_escapes => out.push_str("\\Z"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Escape the `LIKE` wildcards (`%`, `_`) and the escape character itself with a backslash.
/// Must be used together with `ESCAPE '\'` (see [`like_clause`]).
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn like_clause(col: &str, negate: bool, pattern: &str, nb: bool) -> String {
    format!(
        "CAST({col} AS CHAR) {}LIKE {} ESCAPE {}",
        if negate { "NOT " } else { "" },
        quote_literal(pattern, nb),
        quote_literal("\\", nb)
    )
}

fn single(f: &ColumnFilter) -> Result<&str> {
    f.values.first().map(String::as_str).ok_or_else(|| {
        Error::Other(format!(
            "filter on column `{}` requires a value",
            f.column.replace('`', "")
        ))
    })
}

/// SQL condition for one filter.
pub fn filter_condition(f: &ColumnFilter, nb: bool) -> Result<String> {
    let col = quote_ident(&f.column);
    let cmp = |op: &str| -> Result<String> {
        Ok(format!("{col} {op} {}", quote_literal(single(f)?, nb)))
    };
    match f.op {
        FilterOp::Eq => cmp("="),
        FilterOp::Ne => cmp("<>"),
        FilterOp::Lt => cmp("<"),
        FilterOp::Le => cmp("<="),
        FilterOp::Gt => cmp(">"),
        FilterOp::Ge => cmp(">="),
        FilterOp::Contains => Ok(like_clause(
            &col,
            false,
            &format!("%{}%", escape_like(single(f)?)),
            nb,
        )),
        FilterOp::NotContains => Ok(like_clause(
            &col,
            true,
            &format!("%{}%", escape_like(single(f)?)),
            nb,
        )),
        FilterOp::StartsWith => Ok(like_clause(
            &col,
            false,
            &format!("{}%", escape_like(single(f)?)),
            nb,
        )),
        FilterOp::EndsWith => Ok(like_clause(
            &col,
            false,
            &format!("%{}", escape_like(single(f)?)),
            nb,
        )),
        FilterOp::IsNull => Ok(format!("{col} IS NULL")),
        FilterOp::IsNotNull => Ok(format!("{col} IS NOT NULL")),
        FilterOp::In => {
            if f.values.is_empty() {
                Ok("(1 = 0)".to_string())
            } else {
                let list: Vec<String> = f.values.iter().map(|v| quote_literal(v, nb)).collect();
                Ok(format!("{col} IN ({})", list.join(", ")))
            }
        }
    }
}

/// Full `WHERE ...` clause (with a leading space) or an empty string.
///
/// `raw_where` is appended verbatim inside parentheses; a newline precedes the closing
/// parenthesis so a trailing `-- comment` cannot swallow it.
pub fn where_clause(filters: &[ColumnFilter], raw_where: Option<&str>, nb: bool) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for f in filters {
        parts.push(filter_condition(f, nb)?);
    }
    if let Some(raw) = raw_where.map(str::trim).filter(|r| !r.is_empty()) {
        parts.push(format!("({raw}\n)"));
    }
    if parts.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(" WHERE {}", parts.join(" AND ")))
    }
}

/// `ORDER BY ...` clause (with a leading space) or an empty string. `tie_break` columns (key
/// columns) are appended unless already present in `sort`, making paging deterministic.
pub fn order_clause(sort: &[SortKey], tie_break: &[String]) -> String {
    let mut keys: Vec<String> = sort
        .iter()
        .map(|k| {
            format!(
                "{} {}",
                quote_ident(&k.column),
                if k.descending { "DESC" } else { "ASC" }
            )
        })
        .collect();
    for col in tie_break {
        if !sort.iter().any(|k| k.column.eq_ignore_ascii_case(col)) {
            keys.push(format!("{} ASC", quote_ident(col)));
        }
    }
    if keys.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", keys.join(", "))
    }
}

/// `SELECT *` statement for a page of table data.
pub fn select_sql(
    db: &str,
    table: &str,
    req: &TableDataRequest,
    nb: bool,
    tie_break: &[String],
) -> Result<String> {
    Ok(format!(
        "SELECT * FROM {}{}{} LIMIT {} OFFSET {}",
        quote_table(db, table),
        where_clause(&req.filters, req.raw_where.as_deref(), nb)?,
        order_clause(&req.sort, tie_break),
        req.limit,
        req.offset
    ))
}

/// `SELECT COUNT(*)` statement.
pub fn count_sql(db: &str, table: &str, req: &TableCountRequest, nb: bool) -> Result<String> {
    Ok(format!(
        "SELECT COUNT(*) FROM {}{}",
        quote_table(db, table),
        where_clause(&req.filters, req.raw_where.as_deref(), nb)?
    ))
}

/// Validate a user-typed `WHERE` body before it is embedded in a statement.
///
/// The text must be a single expression: it is rejected when it contains a `;` outside string
/// literals / identifiers / comments, unbalanced or extra parentheses, or an unterminated
/// string, identifier or block comment. Understands `'..'` and `".."` (backslash escapes unless
/// `no_backslash_escapes`), backticks, `-- ` and `#` line comments, `/* */` comments and the
/// executable comments `/*! */`, `/*M! */` whose content is lexed as ordinary SQL.
pub fn check_raw_where(raw: &str, no_backslash_escapes: bool) -> Result<()> {
    let fail = |why: &str| Err(Error::query(format!("raw WHERE rejected: {why}")));
    let c: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut depth: i64 = 0;
    let mut exec_comments = 0usize;
    while i < c.len() {
        match c[i] {
            q @ ('\'' | '"' | '`') => {
                i += 1;
                loop {
                    let Some(&ch) = c.get(i) else {
                        return fail("unterminated quoted text");
                    };
                    if ch == '\\' && q != '`' && !no_backslash_escapes {
                        i += 2;
                    } else if ch == q {
                        if c.get(i + 1) == Some(&q) {
                            i += 2; // doubled quote
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            '-' if c.get(i + 1) == Some(&'-')
                && c.get(i + 2)
                    .is_none_or(|x| x.is_whitespace() || x.is_control()) =>
            {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '#' => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                let exec = c.get(i + 2) == Some(&'!')
                    || (c.get(i + 2) == Some(&'M') && c.get(i + 3) == Some(&'!'));
                if exec {
                    exec_comments += 1;
                    i += if c[i + 2] == '!' { 3 } else { 4 };
                    while c.get(i).is_some_and(char::is_ascii_digit) {
                        i += 1;
                    }
                } else {
                    i += 2;
                    loop {
                        match (c.get(i), c.get(i + 1)) {
                            (Some('*'), Some('/')) => {
                                i += 2;
                                break;
                            }
                            (Some(_), _) => i += 1,
                            (None, _) => return fail("unterminated comment"),
                        }
                    }
                }
            }
            '*' if exec_comments > 0 && c.get(i + 1) == Some(&'/') => {
                exec_comments -= 1;
                i += 2;
            }
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return fail("unbalanced closing parenthesis");
                }
                i += 1;
            }
            ';' => return fail("`;` (multiple statements) is not allowed"),
            _ => i += 1,
        }
    }
    if exec_comments > 0 {
        return fail("unterminated executable comment");
    }
    if depth != 0 {
        return fail("unbalanced parentheses");
    }
    Ok(())
}

/// Resolve the `(database, table)` of a table or view path. Folder segments are ignored so both
/// `[Database, Table]` and `[Database, Folder(tables), Table]` work.
pub fn table_target(path: &ObjectPath) -> Result<(String, String)> {
    let segs: Vec<_> = path
        .segments
        .iter()
        .filter(|s| s.kind != ObjectKind::Folder)
        .collect();
    match segs.as_slice() {
        [db, t]
            if db.kind == ObjectKind::Database
                && matches!(t.kind, ObjectKind::Table | ObjectKind::View) =>
        {
            Ok((db.name.clone(), t.name.clone()))
        }
        _ => Err(Error::Other(
            "path does not identify a table or view (expected database/table)".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn f(col: &str, op: FilterOp, vals: &[&str]) -> ColumnFilter {
        ColumnFilter {
            column: col.into(),
            op,
            values: vals.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn identifiers() {
        assert_eq!(quote_ident("a"), "`a`");
        assert_eq!(quote_ident("My Table"), "`My Table`");
        assert_eq!(quote_ident("we`ird"), "`we``ird`");
        assert_eq!(quote_ident("`"), "````");
        assert_eq!(quote_table("d`b", "t"), "`d``b`.`t`");
        assert_eq!(quote_ident("名前"), "`名前`");
    }

    #[test]
    fn literals() {
        assert_eq!(quote_literal("abc", false), "'abc'");
        assert_eq!(quote_literal("it's", false), "'it''s'");
        assert_eq!(quote_literal("a\\b", false), "'a\\\\b'");
        assert_eq!(quote_literal("a\\b", true), "'a\\b'");
        assert_eq!(
            quote_literal("x'; DROP TABLE x; --", false),
            "'x''; DROP TABLE x; --'"
        );
        assert_eq!(
            quote_literal("\\'; DROP TABLE x; --", false),
            "'\\\\''; DROP TABLE x; --'"
        );
        assert_eq!(quote_literal("\\'", true), "'\\'''");
        assert_eq!(quote_literal("a\0b\n\r\x1a", false), "'a\\0b\\n\\r\\Z'");
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like("50%_off\\"), "50\\%\\_off\\\\");
        let c = filter_condition(&f("n", FilterOp::Contains, &["a%b_c"]), false).unwrap();
        assert_eq!(c, "CAST(`n` AS CHAR) LIKE '%a\\\\%b\\\\_c%' ESCAPE '\\\\'");
        let c = filter_condition(&f("n", FilterOp::Contains, &["a%b_c"]), true).unwrap();
        assert_eq!(c, "CAST(`n` AS CHAR) LIKE '%a\\%b\\_c%' ESCAPE '\\'");
        let c = filter_condition(&f("n", FilterOp::StartsWith, &["x"]), false).unwrap();
        assert!(c.contains("LIKE 'x%'"));
        let c = filter_condition(&f("n", FilterOp::EndsWith, &["x"]), false).unwrap();
        assert!(c.contains("LIKE '%x'"));
        let c = filter_condition(&f("n", FilterOp::NotContains, &["x"]), false).unwrap();
        assert!(c.contains("NOT LIKE '%x%'"));
    }

    #[test]
    fn comparison_and_in() {
        let ops = [
            (FilterOp::Eq, "="),
            (FilterOp::Ne, "<>"),
            (FilterOp::Lt, "<"),
            (FilterOp::Le, "<="),
            (FilterOp::Gt, ">"),
            (FilterOp::Ge, ">="),
        ];
        for (op, sym) in ops {
            assert_eq!(
                filter_condition(&f("a`b", op, &["1"]), false).unwrap(),
                format!("`a``b` {sym} '1'")
            );
        }
        assert_eq!(
            filter_condition(&f("a", FilterOp::In, &["1", "x'y"]), false).unwrap(),
            "`a` IN ('1', 'x''y')"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::In, &[]), false).unwrap(),
            "(1 = 0)"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::IsNull, &["ignored"]), false).unwrap(),
            "`a` IS NULL"
        );
        assert_eq!(
            filter_condition(&f("a", FilterOp::IsNotNull, &[]), false).unwrap(),
            "`a` IS NOT NULL"
        );
        assert!(filter_condition(&f("a", FilterOp::Eq, &[]), false).is_err());
    }

    #[test]
    fn attack_strings_stay_inside_literals() {
        let evil = "'; DROP TABLE x; --";
        let sql = where_clause(&[f("c", FilterOp::Eq, &[evil])], None, false).unwrap();
        assert_eq!(sql, " WHERE `c` = '''; DROP TABLE x; --'");
        let evil2 = "\\'; DROP TABLE x; --";
        let sql = where_clause(&[f("c", FilterOp::Eq, &[evil2])], None, false).unwrap();
        // The backslash is doubled, so the following quote is doubled-quote, not an escape.
        assert_eq!(sql, " WHERE `c` = '\\\\''; DROP TABLE x; --'");
        let sql = where_clause(
            &[f("c`; DROP TABLE x; --", FilterOp::IsNull, &[])],
            None,
            false,
        )
        .unwrap();
        assert_eq!(sql, " WHERE `c``; DROP TABLE x; --` IS NULL");
    }

    #[test]
    fn where_and_raw() {
        assert_eq!(where_clause(&[], None, false).unwrap(), "");
        assert_eq!(where_clause(&[], Some("   "), false).unwrap(), "");
        let sql = where_clause(
            &[f("a", FilterOp::Eq, &["1"]), f("b", FilterOp::IsNull, &[])],
            Some("c > 3 OR d = 1 -- x"),
            false,
        )
        .unwrap();
        assert_eq!(
            sql,
            " WHERE `a` = '1' AND `b` IS NULL AND (c > 3 OR d = 1 -- x\n)"
        );
    }

    #[test]
    fn select_and_count() {
        let req = TableDataRequest {
            query_id: Uuid::new_v4(),
            table: ObjectPath::root(),
            filters: vec![f("a", FilterOp::Gt, &["5"])],
            raw_where: Some("b = 2".into()),
            sort: vec![
                SortKey {
                    column: "a".into(),
                    descending: true,
                },
                SortKey {
                    column: "b c".into(),
                    descending: false,
                },
            ],
            offset: 20,
            limit: 10,
            batch_size: None,
        };
        assert_eq!(
            select_sql("d", "T", &req, false, &["a".into(), "id".into()]).unwrap(),
            "SELECT * FROM `d`.`T` WHERE `a` > '5' AND (b = 2\n) ORDER BY `a` DESC, `b c` ASC, `id` ASC LIMIT 10 OFFSET 20"
        );
        let creq = TableCountRequest {
            query_id: None,
            table: ObjectPath::root(),
            filters: vec![],
            raw_where: None,
        };
        assert_eq!(
            count_sql("d", "T", &creq, false).unwrap(),
            "SELECT COUNT(*) FROM `d`.`T`"
        );
    }

    #[test]
    fn tie_break_columns() {
        assert_eq!(order_clause(&[], &[]), "");
        assert_eq!(
            order_clause(&[], &["id".into(), "b`".into()]),
            " ORDER BY `id` ASC, `b``` ASC"
        );
        let sort = [SortKey {
            column: "ID".into(),
            descending: true,
        }];
        assert_eq!(
            order_clause(&sort, &["id".into(), "k".into()]),
            " ORDER BY `ID` DESC, `k` ASC"
        );
    }

    #[test]
    fn raw_where_lexer() {
        let ok = [
            "a = 1",
            "name = 'a;b'",
            "name = \"x;y\" AND (b = 2)",
            "n = 'it''s ; fine'",
            "n = 'back\\'; still string'",
            "`we;ird` = 1",
            "`a``;b` = 1",
            "a = 1 -- trailing; comment",
            "a = 1 # trailing; comment",
            "a = 1 /* ; ) ( */",
            "a = /*! 1 */ 1",
            "a = 1 --x",
            "((a)) OR (b)",
        ];
        for w in ok {
            assert!(check_raw_where(w, false).is_ok(), "should accept: {w}");
        }
        let bad = [
            "1=1); COMMIT; DELETE FROM d.t; SELECT (1",
            "1=1; DROP TABLE t",
            "1=1);",
            "(a = 1",
            "a = 1)",
            "a = 'unterminated",
            "a = `unterminated",
            "a = 1 /* open",
            "a = 1 /*! SELECT 1; */",
            "a = 1 /*! (",
            "a = 'x\\' ; DROP TABLE t; --",
            "a = 1 --;DROP", // `--;` is not a comment: the `;` is live
        ];
        for w in bad {
            assert!(check_raw_where(w, false).is_err(), "should reject: {w}");
        }
        // Backslash is an ordinary character under NO_BACKSLASH_ESCAPES.
        assert!(check_raw_where("a = 'x\\'", true).is_ok());
        assert!(check_raw_where("a = 'x\\'", false).is_err());
        assert!(check_raw_where("a = 'x\\' ; --", true).is_err());
        assert!(matches!(
            check_raw_where("1; 2", false),
            Err(Error::Query { position: None, .. })
        ));
    }

    #[test]
    fn table_target_paths() {
        let p = ObjectPath::root()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Table, "t");
        assert_eq!(table_target(&p).unwrap(), ("db".into(), "t".into()));
        let p = ObjectPath::root()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Folder, "views")
            .child(ObjectKind::View, "v");
        assert_eq!(table_target(&p).unwrap(), ("db".into(), "v".into()));
        assert!(table_target(&ObjectPath::root()).is_err());
        assert!(table_target(&ObjectPath::root().child(ObjectKind::Database, "db")).is_err());
    }
}
