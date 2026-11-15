//! Static recognition of "plain" `SELECT`s, to find the table column each result column comes
//! from when the driver cannot tell (SQL Server, Oracle, CQL, ClickHouse).
//!
//! Only this shape is recognised:
//!
//! ```text
//! SELECT [TOP n [PERCENT]] <* | alias.* | column list> FROM <one table> [[AS] alias]
//!        [WITH (hints)] [WHERE ...] [ORDER BY ...] [LIMIT / OFFSET / FETCH ...]
//! ```
//!
//! A column is a bare (optionally qualified, optionally quoted) identifier with an optional
//! alias; anything else in the list (expressions, functions, constants) yields no origin. No
//! `JOIN`, comma join, `GROUP BY`, `HAVING`, `DISTINCT`, set operation or sub select in `FROM`.
//! When in doubt the answer is "no origin": a wrong origin would write to the wrong table.

use dbstudio_core::{ColumnMeta, ColumnOrigin, ScriptDialect};

use crate::lexer::{Token, TokenKind, tokenize};

/// A table reference as typed: 1 to 3 dotted parts, already unquoted and case folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableName {
    /// Parts, outermost first (`db.schema.table`).
    pub parts: Vec<String>,
}

/// One item of the select list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectItem {
    /// A bare column, `name` as typed (folded) and its alias, when there is one.
    Column {
        /// Column name as typed, folded like the dialect folds unquoted names.
        name: String,
        /// `AS alias` / bare alias, folded.
        alias: Option<String>,
    },
    /// An expression, function call, constant... (no origin).
    Other,
}

/// What follows `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectList {
    /// `*` or `alias.*`.
    Star,
    /// An explicit list.
    Items(Vec<SelectItem>),
}

/// A recognised plain select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleSelect {
    /// The one table read.
    pub table: TableName,
    /// The select list.
    pub list: SelectList,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: TokenKind,
    text: String,
    lower: String,
    /// Identifier text without quotes (`Word`/`QuotedIdent` only).
    quoted: bool,
}

impl Tok {
    fn is_word(&self, w: &str) -> bool {
        self.kind == TokenKind::Word && self.lower == w
    }
    fn is_punct(&self, c: char) -> bool {
        self.kind == TokenKind::Punct && self.text.len() == 1 && self.text.starts_with(c)
    }
    fn is_ident(&self) -> bool {
        self.kind == TokenKind::QuotedIdent
            || (self.kind == TokenKind::Word && !RESERVED.contains(&self.lower.as_str()))
    }
}

/// Words that can never be a column or an alias.
const RESERVED: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "not",
    "in",
    "is",
    "null",
    "like",
    "between",
    "exists",
    "case",
    "when",
    "then",
    "else",
    "end",
    "as",
    "on",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "natural",
    "group",
    "order",
    "by",
    "having",
    "limit",
    "offset",
    "union",
    "intersect",
    "except",
    "minus",
    "all",
    "distinct",
    "true",
    "false",
    "fetch",
    "for",
    "with",
    "window",
    "into",
    "top",
    "prewhere",
    "settings",
    "format",
    "sample",
    "final",
    "using",
    "lateral",
    "apply",
    "pivot",
    "unpivot",
    "connect",
    "start",
    "model",
    "qualify",
    "allow",
    "per",
    "current_date",
    "current_time",
    "current_timestamp",
    "sysdate",
    "systimestamp",
    "rownum",
    "rowid",
    "level",
    "user",
    "uid",
    "current_user",
    "session_user",
];

/// Keywords that may follow the table (and its alias) in a plain select.
const TAIL_START: &[&str] = &[
    "where", "order", "limit", "offset", "fetch", "prewhere", "settings", "format", "final",
    "sample", "with", "for", "allow",
];

/// Keywords which, anywhere at the top level of the tail, make the select non-plain.
const TAIL_FORBIDDEN: &[&str] = &[
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "group",
    "having",
    "union",
    "intersect",
    "except",
    "minus",
    "window",
    "lateral",
    "apply",
    "pivot",
    "unpivot",
    "connect",
    "qualify",
    "into",
];

fn fold(d: ScriptDialect, t: &Tok) -> String {
    let inner = unquote(t);
    if t.quoted {
        return inner;
    }
    match d {
        ScriptDialect::Oracle => inner.to_uppercase(),
        ScriptDialect::Cql => inner.to_lowercase(),
        _ => inner,
    }
}

fn unquote(t: &Tok) -> String {
    if !t.quoted {
        return t.text.clone();
    }
    let chars: Vec<char> = t.text.chars().collect();
    if chars.len() < 2 {
        return t.text.clone();
    }
    let (open, close) = (chars[0], chars[chars.len() - 1]);
    let inner: String = chars[1..chars.len() - 1].iter().collect();
    match (open, close) {
        ('"', '"') => inner.replace("\"\"", "\""),
        ('`', '`') => inner.replace("``", "`"),
        ('[', ']') => inner.replace("]]", "]"),
        _ => inner,
    }
}

fn lex(d: ScriptDialect, text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let tokens: Vec<Token> = tokenize(d, text);
    tokens
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| {
            let s: String = chars
                .get(t.range.start as usize..t.range.end as usize)
                .map(|c| c.iter().collect())
                .unwrap_or_default();
            Tok {
                kind: t.kind,
                lower: s.to_lowercase(),
                quoted: t.kind == TokenKind::QuotedIdent,
                text: s,
            }
        })
        .collect()
}

/// Recognise a plain select, or `None`.
pub fn parse_simple_select(d: ScriptDialect, text: &str) -> Option<SimpleSelect> {
    if !matches!(
        d,
        ScriptDialect::Postgres
            | ScriptDialect::MySql { .. }
            | ScriptDialect::MsSql
            | ScriptDialect::Oracle
            | ScriptDialect::Cql
            | ScriptDialect::ClickHouse
    ) {
        return None;
    }
    let mut toks = lex(d, text);
    // A trailing statement terminator is fine; a second statement is not.
    while toks.last().is_some_and(|t| t.is_punct(';')) {
        toks.pop();
    }
    if toks.iter().any(|t| t.is_punct(';')) {
        return None;
    }
    let mut i = 0;
    if !toks.get(i)?.is_word("select") {
        return None;
    }
    i += 1;
    // TOP n [PERCENT] (T-SQL).
    if toks.get(i).is_some_and(|t| t.is_word("top")) {
        if d != ScriptDialect::MsSql {
            return None;
        }
        i += 1;
        match toks.get(i)? {
            t if t.kind == TokenKind::Number => i += 1,
            t if t.is_punct('(') => i = skip_group(&toks, i)?,
            _ => return None,
        }
        if toks.get(i).is_some_and(|t| t.is_word("percent")) {
            i += 1;
        }
        if toks.get(i).is_some_and(|t| t.is_word("with")) {
            return None; // WITH TIES
        }
    }
    if toks.get(i).is_some_and(|t| t.is_word("all")) {
        i += 1;
    }
    // Select list up to the top-level FROM.
    let list_start = i;
    let mut depth = 0i32;
    let mut from = None;
    while let Some(t) = toks.get(i) {
        if t.is_punct('(') {
            depth += 1;
        } else if t.is_punct(')') {
            depth -= 1;
            if depth < 0 {
                return None;
            }
        } else if depth == 0 && t.is_word("from") {
            from = Some(i);
            break;
        } else if depth == 0 && (t.is_word("distinct") || t.is_word("select")) {
            return None;
        }
        i += 1;
    }
    let from = from?;
    let list_toks = &toks[list_start..from];
    if list_toks.is_empty() {
        return None;
    }
    // FROM <table> [[AS] alias]
    let mut j = from + 1;
    let mut parts = Vec::new();
    loop {
        let t = toks.get(j)?;
        if !t.is_ident() {
            return None; // sub select, function, ...
        }
        parts.push(fold(d, t));
        j += 1;
        if toks.get(j).is_some_and(|t| t.is_punct('.')) {
            j += 1;
        } else {
            break;
        }
    }
    let max_parts = if d == ScriptDialect::MsSql { 3 } else { 2 };
    if parts.len() > max_parts {
        return None;
    }
    let mut alias = None;
    if toks.get(j).is_some_and(|t| t.is_word("as")) {
        let t = toks.get(j + 1)?;
        if !t.is_ident() {
            return None;
        }
        alias = Some(fold(d, t));
        j += 2;
    } else if let Some(t) = toks.get(j).filter(|t| {
        t.is_ident() && !(t.kind == TokenKind::Word && TAIL_START.contains(&t.lower.as_str()))
    }) {
        alias = Some(fold(d, t));
        j += 1;
    }
    // Tail: starts with a clause keyword (or nothing), no join / group / set operation.
    if let Some(t) = toks.get(j) {
        if !(t.kind == TokenKind::Word && TAIL_START.contains(&t.lower.as_str())) {
            return None;
        }
        // T-SQL table hints: WITH (NOLOCK)
        if t.is_word("with") && !toks.get(j + 1).is_some_and(|n| n.is_punct('(')) {
            return None;
        }
    }
    let mut depth = 0i32;
    for t in &toks[j..] {
        if t.is_punct('(') {
            depth += 1;
        } else if t.is_punct(')') {
            depth -= 1;
        } else if depth == 0 {
            if t.kind == TokenKind::Word && TAIL_FORBIDDEN.contains(&t.lower.as_str()) {
                return None;
            }
            // A comma at the top level of WHERE/ORDER BY is fine (ORDER BY a, b; LIMIT a, b), but
            // before any clause keyword it would be a comma join (handled above: the tail must
            // start with a clause keyword).
        }
    }
    if depth != 0 {
        return None;
    }
    let table_ref_names: Vec<&str> = {
        let mut v = vec![parts.last()?.as_str()];
        if let Some(a) = &alias {
            v.insert(0, a.as_str());
        }
        v
    };
    // Qualifier a column may carry: the alias when there is one, else the table name.
    let qualifier = table_ref_names[0].to_string();
    let list = parse_list(d, list_toks, &qualifier, alias.is_some(), &parts)?;
    Some(SimpleSelect {
        table: TableName { parts },
        list,
    })
}

/// Index just past the group opened at `open` (a `(`).
fn skip_group(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, t) in toks.iter().enumerate().skip(open) {
        if t.is_punct('(') {
            depth += 1;
        } else if t.is_punct(')') {
            depth -= 1;
            if depth == 0 {
                return Some(k + 1);
            }
        }
    }
    None
}

fn parse_list(
    d: ScriptDialect,
    list: &[Tok],
    qualifier: &str,
    has_alias: bool,
    table_parts: &[String],
) -> Option<SelectList> {
    // Split at top-level commas.
    let mut items: Vec<&[Tok]> = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (k, t) in list.iter().enumerate() {
        if t.is_punct('(') {
            depth += 1;
        } else if t.is_punct(')') {
            depth -= 1;
        } else if depth == 0 && t.is_punct(',') {
            items.push(&list[start..k]);
            start = k + 1;
        }
    }
    items.push(&list[start..]);
    // `*` or `q.*` alone.
    if items.len() == 1 {
        let it = items[0];
        if it.len() == 1 && it[0].is_punct('*') {
            return Some(SelectList::Star);
        }
        if it.len() >= 3 && it[it.len() - 1].is_punct('*') && it[it.len() - 2].is_punct('.') {
            let q = &it[..it.len() - 2];
            if qualifier_matches(d, q, qualifier, has_alias, table_parts) {
                return Some(SelectList::Star);
            }
            return None;
        }
    }
    let parsed = items
        .into_iter()
        .map(|it| parse_item(d, it, qualifier, has_alias, table_parts))
        .collect();
    Some(SelectList::Items(parsed))
}

/// `a.b.c` parts of a qualifier (everything before the column) matching the table.
fn qualifier_matches(
    d: ScriptDialect,
    q: &[Tok],
    qualifier: &str,
    has_alias: bool,
    table_parts: &[String],
) -> bool {
    // q is `ident (. ident)*`
    let mut names = Vec::new();
    let mut k = 0;
    while k < q.len() {
        if !q[k].is_ident() {
            return false;
        }
        names.push(fold(d, &q[k]));
        k += 1;
        if k < q.len() {
            if !q[k].is_punct('.') {
                return false;
            }
            k += 1;
            if k == q.len() {
                return false;
            }
        }
    }
    if has_alias {
        // Only the alias qualifies a column once the table has one.
        names.len() == 1 && names[0].eq_ignore_ascii_case(qualifier)
    } else {
        // The table name, possibly qualified like in FROM (suffix of the parts).
        names.len() <= table_parts.len()
            && names
                .iter()
                .rev()
                .zip(table_parts.iter().rev())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

fn parse_item(
    d: ScriptDialect,
    it: &[Tok],
    qualifier: &str,
    has_alias: bool,
    table_parts: &[String],
) -> SelectItem {
    // Optional alias: `... AS alias` or `... alias` (two identifiers in a row at the end).
    let mut body = it;
    let mut alias = None;
    if body.len() >= 3 && body[body.len() - 2].is_word("as") && body[body.len() - 1].is_ident() {
        alias = Some(fold(d, &body[body.len() - 1]));
        body = &body[..body.len() - 2];
    } else if body.len() >= 2
        && body[body.len() - 1].is_ident()
        && (body[body.len() - 2].is_ident())
        && !body[body.len() - 2].is_punct('.')
    {
        alias = Some(fold(d, &body[body.len() - 1]));
        body = &body[..body.len() - 1];
    }
    // body: `ident` or `qualifier . ident`
    let (q, col): (&[Tok], &Tok) = match body {
        [c] if c.is_ident() => (&[], c),
        [.., dot, c] if c.is_ident() && dot.is_punct('.') => (&body[..body.len() - 2], c),
        _ => return SelectItem::Other,
    };
    if !q.is_empty() && !qualifier_matches(d, q, qualifier, has_alias, table_parts) {
        return SelectItem::Other;
    }
    SelectItem::Column {
        name: fold(d, col),
        alias,
    }
}

/// Where the parts of a table name go for `d`: `(database, schema, table)`.
fn split_parts(d: ScriptDialect, parts: &[String]) -> (Option<String>, Option<String>, String) {
    let table = parts.last().cloned().unwrap_or_default();
    match (d, parts.len()) {
        (_, 1) => (None, None, table),
        (ScriptDialect::MySql { .. } | ScriptDialect::ClickHouse | ScriptDialect::Cql, _) => {
            (Some(parts[0].clone()), None, table)
        }
        (_, 2) => (None, Some(parts[0].clone()), table),
        (_, _) => (Some(parts[0].clone()), Some(parts[1].clone()), table),
    }
}

/// Origin of each result column of the plain select `text`, or `None` when `text` is not a plain
/// select (or its list does not line up with `columns`).
///
/// A column without an alias keeps the name the server reported (its real name); an aliased one
/// is the name as typed, folded like the dialect folds unquoted identifiers.
pub fn infer_origins(
    d: ScriptDialect,
    text: &str,
    columns: &[ColumnMeta],
) -> Option<Vec<Option<ColumnOrigin>>> {
    let sel = parse_simple_select(d, text)?;
    let (database, schema, table) = split_parts(d, &sel.table.parts);
    let origin = |column: String| ColumnOrigin {
        database: database.clone(),
        schema: schema.clone(),
        table: table.clone(),
        column,
    };
    match sel.list {
        SelectList::Star => Some(
            columns
                .iter()
                .map(|c| Some(origin(c.name.clone())))
                .collect(),
        ),
        SelectList::Items(items) => {
            if items.len() != columns.len() {
                return None;
            }
            Some(
                items
                    .into_iter()
                    .zip(columns)
                    .map(|(item, col)| match item {
                        SelectItem::Other => None,
                        SelectItem::Column { name, alias } => {
                            // The server's name must be what we parsed, or the parse is wrong.
                            let shown = alias.as_ref().unwrap_or(&name);
                            if !shown.eq_ignore_ascii_case(&col.name) {
                                return None;
                            }
                            Some(origin(if alias.is_some() {
                                name
                            } else {
                                col.name.clone()
                            }))
                        }
                    })
                    .collect(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ValueKind;

    fn cols(names: &[&str]) -> Vec<ColumnMeta> {
        names
            .iter()
            .map(|n| ColumnMeta {
                name: (*n).into(),
                type_name: "x".into(),
                value_kind: ValueKind::Text,
                nullable: None,
                origin: None,
            })
            .collect()
    }

    fn infer(d: ScriptDialect, sql: &str, names: &[&str]) -> Option<Vec<Option<String>>> {
        infer_origins(d, sql, &cols(names)).map(|v| {
            v.into_iter()
                .map(|o| o.map(|o| format!("{}.{}", o.table, o.column)))
                .collect()
        })
    }

    fn s(v: &[&str]) -> Option<Vec<Option<String>>> {
        Some(v.iter().map(|x| Some((*x).to_string())).collect())
    }

    const MS: ScriptDialect = ScriptDialect::MsSql;
    const CH: ScriptDialect = ScriptDialect::ClickHouse;

    #[test]
    fn plain_columns_with_table_alias_and_as() {
        assert_eq!(
            infer(
                MS,
                "SELECT u.id, u.name AS n, email e FROM dbo.users u WHERE u.id > 3",
                &["id", "n", "e"]
            ),
            s(&["users.id", "users.name", "users.email"])
        );
        assert_eq!(
            infer(MS, "select id from users as x", &["id"]),
            s(&["users.id"])
        );
    }

    #[test]
    fn star_uses_the_result_names() {
        assert_eq!(
            infer(CH, "SELECT * FROM db.t", &["a", "B"]),
            s(&["t.a", "t.B"])
        );
        assert_eq!(
            infer(CH, "SELECT t.* FROM t ORDER BY a LIMIT 5", &["a"]),
            s(&["t.a"])
        );
        assert_eq!(infer(CH, "SELECT x.* FROM t x", &["a"]), s(&["t.a"]));
        // The table name no longer qualifies once an alias exists.
        assert_eq!(infer(CH, "SELECT t.* FROM t x", &["a"]), None);
    }

    #[test]
    fn top_limit_fetch_and_trailing_semicolon() {
        assert_eq!(infer(MS, "SELECT TOP 10 id FROM t;", &["id"]), s(&["t.id"]));
        assert_eq!(
            infer(MS, "SELECT TOP (10) PERCENT id FROM t", &["id"]),
            s(&["t.id"])
        );
        assert_eq!(
            infer(MS, "SELECT TOP 5 WITH TIES id FROM t ORDER BY id", &["id"]),
            None
        );
        assert_eq!(
            infer(CH, "SELECT id FROM t LIMIT 10 OFFSET 5", &["id"]),
            s(&["t.id"])
        );
        assert_eq!(
            infer(
                ScriptDialect::Oracle,
                "SELECT id FROM app.t ORDER BY id FETCH FIRST 5 ROWS ONLY",
                &["ID"]
            ),
            s(&["T.ID"])
        );
        assert_eq!(
            infer(MS, "SELECT id FROM t WITH (NOLOCK) WHERE id = 1", &["id"]),
            s(&["t.id"])
        );
    }

    #[test]
    fn schema_and_database_parts_follow_the_dialect() {
        let o = infer_origins(MS, "SELECT id FROM app.dbo.t", &cols(&["id"])).unwrap();
        let o = o[0].clone().unwrap();
        assert_eq!(
            (o.database.as_deref(), o.schema.as_deref(), o.table.as_str()),
            (Some("app"), Some("dbo"), "t")
        );
        let o = infer_origins(MS, "SELECT id FROM [dbo].[my t]", &cols(&["id"])).unwrap();
        let o = o[0].clone().unwrap();
        assert_eq!(
            (o.schema.as_deref(), o.table.as_str()),
            (Some("dbo"), "my t")
        );
        let o = infer_origins(CH, "SELECT id FROM `db`.t", &cols(&["id"])).unwrap();
        assert_eq!(o[0].clone().unwrap().database.as_deref(), Some("db"));
        let o = infer_origins(
            ScriptDialect::Oracle,
            "SELECT id FROM hr.emp",
            &cols(&["ID"]),
        )
        .unwrap();
        let o = o[0].clone().unwrap();
        assert_eq!((o.schema.as_deref(), o.table.as_str()), (Some("HR"), "EMP"));
        let o = infer_origins(ScriptDialect::Cql, "SELECT Id FROM ks.T", &cols(&["id"])).unwrap();
        let o = o[0].clone().unwrap();
        assert_eq!((o.database.as_deref(), o.table.as_str()), (Some("ks"), "t"));
    }

    #[test]
    fn aliased_column_is_the_typed_name_and_plain_is_the_server_name() {
        let o = infer_origins(
            ScriptDialect::Oracle,
            "SELECT first_name AS fn, last_name FROM emp",
            &cols(&["FN", "LAST_NAME"]),
        )
        .unwrap();
        assert_eq!(o[0].clone().unwrap().column, "FIRST_NAME");
        assert_eq!(o[1].clone().unwrap().column, "LAST_NAME");
        // The server's name disagrees with what was parsed: no origin.
        let o = infer_origins(MS, "SELECT id FROM t", &cols(&["other"])).unwrap();
        assert_eq!(o, vec![None]);
    }

    #[test]
    fn expressions_have_no_origin_but_do_not_spoil_the_rest() {
        assert_eq!(
            infer(
                MS,
                "SELECT id, upper(name), price * 2 AS p2, 1, 'x' FROM t",
                &["id", "", "p2", "", ""]
            ),
            Some(vec![Some("t.id".into()), None, None, None, None])
        );
        assert_eq!(
            infer(MS, "SELECT id, name + 'x' FROM t", &["id", "c"]),
            Some(vec![Some("t.id".into()), None])
        );
    }

    #[test]
    fn rejects_joins_group_by_subqueries_and_set_operations() {
        for sql in [
            "SELECT a.id FROM a JOIN b ON a.id = b.id",
            "SELECT a.id FROM a INNER JOIN b ON a.id = b.id",
            "SELECT a.id FROM a, b",
            "SELECT a.id FROM a LEFT JOIN b ON 1=1",
            "SELECT id, count(*) FROM t GROUP BY id",
            "SELECT id FROM t GROUP BY id HAVING count(*) > 1",
            "SELECT DISTINCT id FROM t",
            "SELECT id FROM (SELECT id FROM t) x",
            "SELECT id FROM t UNION SELECT id FROM u",
            "SELECT id FROM t WHERE id IN (1) UNION ALL SELECT 1",
            "SELECT 1",
            "SELECT id FROM t; SELECT id FROM u",
            "UPDATE t SET a = 1",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "SELECT id FROM f(1)",
            "SELECT id INTO x FROM t",
        ] {
            assert_eq!(infer(MS, sql, &["id", "n"]), None, "{sql}");
        }
    }

    #[test]
    fn a_subquery_in_where_does_not_matter() {
        assert_eq!(
            infer(
                CH,
                "SELECT id FROM t WHERE id IN (SELECT id FROM u JOIN v ON 1 GROUP BY id)",
                &["id"]
            ),
            s(&["t.id"])
        );
    }

    #[test]
    fn column_count_mismatch_and_foreign_qualifier() {
        assert_eq!(infer(MS, "SELECT id FROM t", &["id", "x"]), None);
        assert_eq!(
            infer(MS, "SELECT other.id FROM t", &["id"]),
            Some(vec![None])
        );
    }

    #[test]
    fn comments_and_strings_are_not_keywords() {
        assert_eq!(
            infer(
                MS,
                "SELECT id /* join */ FROM t -- group by\nWHERE n = 'a join b'",
                &["id"]
            ),
            s(&["t.id"])
        );
    }
}
