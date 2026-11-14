//! Parameter discovery and binding (client side substitution of `:name`, `$1`, `?`, `@p`).

use std::collections::{HashMap, HashSet};

use dbstudio_core::{
    Error, ParamInput, ParamRef, ParamStyle, ParamValue, Result, ScriptDialect, TextRange,
};

use crate::dialect::Dialect;
use crate::lexer::{TokenKind, slice, to_u32, tokenize_chars};

/// One replaced region: `[bound_start, bound_end)` of the bound text came from
/// `[orig_start, orig_end)` of the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    /// Start in the bound text.
    pub bound_start: u32,
    /// End (exclusive) in the bound text.
    pub bound_end: u32,
    /// Start in the original text.
    pub orig_start: u32,
    /// End (exclusive) in the original text.
    pub orig_end: u32,
}

/// Maps character offsets of the bound text back to the original text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OffsetMap {
    /// Replaced regions in ascending order of `bound_start`. Empty = identity.
    pub edits: Vec<Edit>,
}

impl OffsetMap {
    /// Identity map.
    pub fn identity() -> Self {
        Self::default()
    }

    /// Offset in the original text for `pos` in the bound text. A position inside a replaced
    /// region maps to the start of the original placeholder.
    pub fn to_original(&self, pos: u32) -> u32 {
        let mut delta: i64 = 0;
        for e in &self.edits {
            if pos < e.bound_start {
                break;
            }
            if pos < e.bound_end {
                return e.orig_start;
            }
            delta = i64::from(e.orig_end) - i64::from(e.bound_end);
        }
        u32::try_from((i64::from(pos) + delta).max(0)).unwrap_or(u32::MAX)
    }
}

/// Text with the parameters substituted plus the map back to the original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundScript {
    /// Text to send to the server.
    pub text: String,
    /// Position mapping.
    pub map: OffsetMap,
}

/// A token with its text, for the parameter scanner.
struct Tok {
    kind: TokenKind,
    start: usize,
    end: usize,
    text: String,
}

fn toks(d: ScriptDialect, chars: &[char]) -> Vec<Tok> {
    tokenize_chars(d, chars)
        .0
        .into_iter()
        .map(|t| Tok {
            kind: t.kind,
            start: t.range.start as usize,
            end: t.range.end as usize,
            text: slice(chars, t.range),
        })
        .collect()
}

fn is_var(x: &Tok) -> bool {
    x.kind == TokenKind::Word && x.text.starts_with('@') && !x.text.starts_with("@@")
}

/// Names declared by `DECLARE @x` and by `CREATE PROCEDURE/FUNCTION` parameter lists (T-SQL).
fn declared_mssql(t: &[Tok]) -> HashSet<String> {
    let sig: Vec<&Tok> = t.iter().filter(|x| !x.kind.is_trivia()).collect();
    let mut out = HashSet::new();
    const STOP: &[&str] = &[
        "SET",
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "EXEC",
        "EXECUTE",
        "PRINT",
        "IF",
        "WHILE",
        "BEGIN",
        "END",
        "WITH",
        "DECLARE",
        "RETURN",
        "OPEN",
        "FETCH",
        "CLOSE",
        "GOTO",
        "USE",
        "CREATE",
        "DROP",
        "ALTER",
        "TRUNCATE",
        "MERGE",
        "RAISERROR",
        "THROW",
        "WAITFOR",
    ];
    let mut i = 0;
    while i < sig.len() {
        if sig[i].kind == TokenKind::Word && sig[i].text.eq_ignore_ascii_case("DECLARE") {
            let mut depth = 0i32;
            let mut expect = true;
            let mut j = i + 1;
            while j < sig.len() {
                let x = sig[j];
                if x.kind == TokenKind::Punct {
                    match x.text.as_str() {
                        "(" => depth += 1,
                        ")" => depth -= 1,
                        ";" if depth <= 0 => break,
                        "," if depth == 0 => expect = true,
                        _ => {}
                    }
                } else if x.kind == TokenKind::Word {
                    if depth == 0
                        && !x.text.starts_with('@')
                        && STOP.contains(&x.text.to_ascii_uppercase().as_str())
                    {
                        break;
                    }
                    if depth == 0 && expect && is_var(x) {
                        out.insert(x.text.to_ascii_lowercase());
                    }
                    expect = false;
                } else {
                    expect = false;
                }
                j += 1;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    // Procedure / function parameters.
    let words: Vec<String> = sig
        .iter()
        .filter(|x| x.kind == TokenKind::Word)
        .take(5)
        .map(|x| x.text.to_ascii_uppercase())
        .collect();
    if matches!(words.first().map(String::as_str), Some("CREATE" | "ALTER"))
        && words
            .iter()
            .any(|w| matches!(w.as_str(), "PROC" | "PROCEDURE" | "FUNCTION"))
    {
        let mut depth = 0i32;
        for x in &sig {
            if x.kind == TokenKind::Punct {
                match x.text.as_str() {
                    "(" => depth += 1,
                    ")" => depth -= 1,
                    _ => {}
                }
            } else if x.kind == TokenKind::Word {
                let up = x.text.to_ascii_uppercase();
                if depth == 0 && (up == "AS" || up == "RETURNS") {
                    break;
                }
                if is_var(x) {
                    out.insert(x.text.to_ascii_lowercase());
                }
            }
        }
    }
    out
}

/// Character ranges of the Oracle statements that start with `CREATE`.
fn oracle_create_ranges(chars: &[char]) -> Vec<(usize, usize)> {
    let d = ScriptDialect::Oracle;
    let mut st = crate::split::SplitState::default();
    let lx = crate::lexer::Lexer::new(chars, d);
    crate::split::scan(d, chars, &mut st, true)
        .0
        .into_iter()
        .filter(|u| {
            let mut i = u.range.start as usize;
            let end = u.range.end as usize;
            while i < end {
                let (k, e, _) = lx.next(i);
                if k == TokenKind::Word {
                    return chars[i..e]
                        .iter()
                        .collect::<String>()
                        .eq_ignore_ascii_case("CREATE");
                }
                if !k.is_trivia() {
                    return false;
                }
                i = e;
            }
            false
        })
        .map(|u| (u.range.start as usize, u.range.end as usize))
        .collect()
}

/// Parameters of `text`, in order of first appearance.
///
/// * PostgreSQL: `:name` (not `::`, not inside `[..]` slices) and `$n`.
/// * MySQL / MariaDB: `:name` and `?` (each `?` is its own parameter `?1`, `?2`...).
/// * T-SQL: `:name` and `@p` (not `@@x`, not declared with `DECLARE` or as a procedure
///   parameter).
/// * Oracle: `:name` / `:1` (not `:new`, `:old`, `:parent`, not inside `CREATE` statements).
///
/// Strings, comments, quoted identifiers and dollar quoted bodies never contribute.
pub fn find_params(d: ScriptDialect, text: &str) -> Vec<ParamRef> {
    let chars: Vec<char> = text.chars().collect();
    find_params_chars(d, &chars)
}

fn find_params_chars(d: ScriptDialect, chars: &[char]) -> Vec<ParamRef> {
    if !matches!(
        d,
        ScriptDialect::Postgres
            | ScriptDialect::MySql { .. }
            | ScriptDialect::MsSql
            | ScriptDialect::Oracle
    ) {
        return Vec::new();
    }
    let t = toks(d, chars);
    let declared = if matches!(d, ScriptDialect::MsSql) {
        declared_mssql(&t)
    } else {
        HashSet::new()
    };
    let create_ranges = if matches!(d, ScriptDialect::Oracle) {
        oracle_create_ranges(chars)
    } else {
        Vec::new()
    };
    let in_create = |pos: usize| create_ranges.iter().any(|&(s, e)| pos >= s && pos < e);

    let range = |s: usize, e: usize| TextRange {
        start: to_u32(s),
        end: to_u32(e),
    };
    let mut found: Vec<(String, ParamStyle, TextRange)> = Vec::new();
    let mut bracket = 0i32;
    let mut question = 0usize;
    for (i, x) in t.iter().enumerate() {
        match (x.kind, d) {
            (TokenKind::Punct, ScriptDialect::Postgres) if x.text == "[" => bracket += 1,
            (TokenKind::Punct, ScriptDialect::Postgres) if x.text == "]" => {
                bracket = (bracket - 1).max(0);
            }
            (TokenKind::Number, ScriptDialect::Postgres) if x.text.starts_with('$') => {
                found.push((x.text.clone(), ParamStyle::Dollar, range(x.start, x.end)));
            }
            (TokenKind::Punct, _) if x.text == ":" => {
                let Some(nx) = t.get(i + 1) else { continue };
                if nx.start != x.end {
                    continue;
                }
                let oracle = matches!(d, ScriptDialect::Oracle);
                let ok = match nx.kind {
                    TokenKind::Word => !nx.text.starts_with(['@', '#']),
                    TokenKind::Number => oracle && nx.text.chars().all(|c| c.is_ascii_digit()),
                    _ => false,
                };
                if !ok || bracket > 0 {
                    continue;
                }
                if oracle {
                    let lower = nx.text.to_ascii_lowercase();
                    if matches!(lower.as_str(), "new" | "old" | "parent") || in_create(x.start) {
                        continue;
                    }
                }
                found.push((
                    format!(":{}", nx.text),
                    ParamStyle::Named,
                    range(x.start, nx.end),
                ));
            }
            (TokenKind::Punct, ScriptDialect::MySql { .. }) if x.text == "?" => {
                question += 1;
                found.push((
                    format!("?{question}"),
                    ParamStyle::Question,
                    range(x.start, x.end),
                ));
            }
            (TokenKind::Word, ScriptDialect::MsSql)
                if is_var(x) && !declared.contains(&x.text.to_ascii_lowercase()) =>
            {
                found.push((x.text.clone(), ParamStyle::At, range(x.start, x.end)));
            }
            _ => {}
        }
    }
    let mut out: Vec<ParamRef> = Vec::new();
    for (name, style, r) in found {
        if let Some(p) = out.iter_mut().find(|p| p.name == name) {
            p.ranges.push(r);
        } else {
            out.push(ParamRef {
                name,
                style,
                ranges: vec![r],
            });
        }
    }
    out
}

/// `^-?\d+(\.\d+)?([eE][-+]?\d+)?$`
fn valid_number(s: &str) -> bool {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    if b.first() == Some(&'-') {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let s = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > s
    };
    if !digits(&mut i) {
        return false;
    }
    if b.get(i) == Some(&'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(b.get(i), Some('e' | 'E')) {
        i += 1;
        if matches!(b.get(i), Some('+' | '-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

fn render(d: ScriptDialect, name: &str, v: &ParamInput) -> Result<String> {
    let q = Dialect::new(d);
    Ok(match v {
        ParamInput::Null => "NULL".to_string(),
        ParamInput::Text(s) => {
            if s.contains('\0') {
                return Err(Error::query(format!(
                    "parameter {name} must not contain a NUL character"
                )));
            }
            q.literal_text(s)
        }
        ParamInput::Number(s) => {
            if !valid_number(s) {
                return Err(Error::query(format!(
                    "parameter {name}: \"{s}\" is not a number"
                )));
            }
            // Parentheses keep `5 - :n` from becoming the comment `5 --1`.
            if s.starts_with('-') {
                format!("({s})")
            } else {
                s.clone()
            }
        }
        ParamInput::Bool(b) => q.literal(&dbstudio_core::Value::Bool(*b)),
        ParamInput::Raw(s) => s.clone(),
    })
}

/// Substitute `params` into `text`. Every parameter of the text needs a value (matched by
/// [`ParamRef::name`]); extra values are ignored. With no `params` the text is returned
/// unchanged with an identity map.
pub fn bind(d: ScriptDialect, text: &str, params: &[ParamValue]) -> Result<BoundScript> {
    if params.is_empty() {
        return Ok(BoundScript {
            text: text.to_string(),
            map: OffsetMap::identity(),
        });
    }
    let chars: Vec<char> = text.chars().collect();
    let refs = find_params_chars(d, &chars);
    let values: HashMap<&str, &ParamInput> =
        params.iter().map(|p| (p.name.as_str(), &p.value)).collect();
    let mut repl: Vec<(usize, usize, String)> = Vec::new();
    for r in &refs {
        let Some(v) = values.get(r.name.as_str()) else {
            return Err(Error::query(format!(
                "missing value for parameter {}",
                r.name
            )));
        };
        let lit = render(d, &r.name, v)?;
        for range in &r.ranges {
            repl.push((range.start as usize, range.end as usize, lit.clone()));
        }
    }
    repl.sort_by_key(|r| r.0);
    let mut out = String::with_capacity(text.len());
    let mut edits = Vec::with_capacity(repl.len());
    let mut pos = 0usize;
    let mut bound_len = 0usize;
    for (s, e, lit) in repl {
        out.extend(&chars[pos..s]);
        bound_len += s - pos;
        let lit_len = lit.chars().count();
        edits.push(Edit {
            bound_start: to_u32(bound_len),
            bound_end: to_u32(bound_len + lit_len),
            orig_start: to_u32(s),
            orig_end: to_u32(e),
        });
        out.push_str(&lit);
        bound_len += lit_len;
        pos = e;
    }
    out.extend(&chars[pos..]);
    Ok(BoundScript {
        text: out,
        map: OffsetMap { edits },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: ScriptDialect = ScriptDialect::Postgres;
    const MY: ScriptDialect = ScriptDialect::mysql(false, true);
    const MS: ScriptDialect = ScriptDialect::MsSql;
    const ORA: ScriptDialect = ScriptDialect::Oracle;

    fn names(d: ScriptDialect, text: &str) -> Vec<String> {
        find_params(d, text).into_iter().map(|p| p.name).collect()
    }

    fn pv(name: &str, value: ParamInput) -> ParamValue {
        ParamValue {
            name: name.into(),
            value,
        }
    }

    #[test]
    fn pg_named_and_dollar() {
        assert_eq!(
            names(PG, "select :a, $1, :b, $2, :a"),
            [":a", "$1", ":b", "$2"]
        );
        let p = find_params(PG, "select :a, :a");
        assert_eq!(p[0].ranges.len(), 2);
        assert_eq!(p[0].style, ParamStyle::Named);
        assert_eq!(p[0].ranges[0], TextRange { start: 7, end: 9 });
    }

    #[test]
    fn pg_ignores_casts_slices_strings_dollar_bodies() {
        assert!(names(PG, "select a::int, b::text").is_empty());
        assert!(names(PG, "select arr[1:2], arr[1:n]").is_empty());
        assert!(names(PG, "select ':a', \":b\", -- :c\n /* :d */ 1").is_empty());
        assert!(names(PG, "do $$ begin x := :a; return $1; end $$").is_empty());
        assert!(names(PG, "select x := 1").is_empty());
        assert!(names(PG, "select 1 where a ? 'b' and c : d").is_empty());
        assert_eq!(names(PG, "select 1::int, :x"), [":x"]);
    }

    #[test]
    fn pg_dollar_param_not_in_dollar_quote() {
        assert_eq!(names(PG, "select $1 from t where a = $$ $2 $$"), ["$1"]);
        assert_eq!(find_params(PG, "select $1")[0].style, ParamStyle::Dollar);
    }

    #[test]
    fn mysql_question_marks_and_named() {
        let p = find_params(MY, "select ?, ?, :x where a := 1 and b = '?'");
        let n: Vec<_> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(n, ["?1", "?2", ":x"]);
        assert_eq!(p[0].style, ParamStyle::Question);
        assert!(names(MY, "set @a := 5").is_empty());
        assert!(names(MY, "select @a, @@global.x").is_empty());
        assert!(names(MY, "select `a?b` # ?\n").is_empty());
    }

    #[test]
    fn mssql_at_params() {
        assert_eq!(
            names(MS, "select * from t where a = @p and b = @q or c = @p"),
            ["@p", "@q"]
        );
        assert!(names(MS, "select @@rowcount, @@version").is_empty());
        assert!(names(MS, "declare @p int = 1; select @p").is_empty());
        assert_eq!(names(MS, "declare @p int; select @p, @q"), ["@q"]);
        assert_eq!(
            names(MS, "declare @a int, @b int; select @a, @b, @c"),
            ["@c"]
        );
        assert_eq!(names(MS, "select :x, '@no', [@no]"), [":x"]);
        assert!(names(MS, "create procedure p @id int as select @id").is_empty());
        assert!(
            names(
                MS,
                "create function f (@id int) returns int as begin return @id end"
            )
            .is_empty()
        );
    }

    #[test]
    fn mssql_declare_is_case_insensitive() {
        assert!(names(MS, "DECLARE @P int; select @p").is_empty());
    }

    #[test]
    fn oracle_binds() {
        assert_eq!(
            names(ORA, "select :a, :1, :b from dual"),
            [":a", ":1", ":b"]
        );
        assert!(names(ORA, "begin :new.x := :old.y; z := :parent.q; end;").is_empty());
        assert_eq!(names(ORA, "begin :x := 1; end;"), [":x"]);
        assert!(
            names(
                ORA,
                "create or replace trigger t before insert on a for each row begin :new.a := :b; end;"
            )
            .is_empty()
        );
        assert_eq!(
            names(ORA, "create table t (a int);\nselect :v from dual"),
            [":v"]
        );
        assert!(names(ORA, "select q'[:a]', ':b' from dual").is_empty());
    }

    #[test]
    fn other_dialects_have_no_params() {
        for d in [
            ScriptDialect::MongoShell,
            ScriptDialect::RedisCli,
            ScriptDialect::Opaque,
        ] {
            assert!(names(d, "select :a, ?, $1, @p").is_empty());
        }
    }

    #[test]
    fn bind_without_params_is_identity() {
        let b = bind(PG, "select 1", &[]).unwrap();
        assert_eq!(b.text, "select 1");
        assert_eq!(b.map, OffsetMap::identity());
        assert_eq!(b.map.to_original(7), 7);
    }

    #[test]
    fn bind_renders_literals_per_dialect() {
        let b = bind(
            PG,
            "select :a, :b, :c, :d, :e",
            &[
                pv(":a", ParamInput::Text("it's".into())),
                pv(":b", ParamInput::Number("-1.5e3".into())),
                pv(":c", ParamInput::Null),
                pv(":d", ParamInput::Bool(true)),
                pv(":e", ParamInput::Raw("now()".into())),
            ],
        )
        .unwrap();
        assert_eq!(b.text, "select 'it''s', (-1.5e3), NULL, true, now()");
        let b = bind(MS, "select @p", &[pv("@p", ParamInput::Text("x".into()))]).unwrap();
        assert_eq!(b.text, "select N'x'");
        let b = bind(
            MY,
            "select ?, ?",
            &[
                pv("?1", ParamInput::Number("1".into())),
                pv("?2", ParamInput::Bool(false)),
            ],
        )
        .unwrap();
        assert_eq!(b.text, "select 1, FALSE");
        let b = bind(
            ORA,
            "select :1 from dual",
            &[pv(":1", ParamInput::Text("a".into()))],
        )
        .unwrap();
        assert_eq!(b.text, "select 'a' from dual");
        let b = bind(
            PG,
            "select $1, $1",
            &[pv("$1", ParamInput::Number("7".into()))],
        )
        .unwrap();
        assert_eq!(b.text, "select 7, 7");
    }

    #[test]
    fn negative_numbers_never_form_a_comment() {
        let b = bind(
            PG,
            "select 5 -:n, 5-:n",
            &[pv(":n", ParamInput::Number("-1".into()))],
        )
        .unwrap();
        assert_eq!(b.text, "select 5 -(-1), 5-(-1)");
        assert_eq!(crate::split(PG, &b.text).len(), 1);
        assert!(crate::analyze(PG, &b.text).problems.is_empty());
    }

    #[test]
    fn postgres_text_with_backslash_is_safe_in_any_mode() {
        let b = bind(
            PG,
            "select :a",
            &[pv(":a", ParamInput::Text("\\'; drop table x; --".into()))],
        )
        .unwrap();
        assert_eq!(b.text, "select E'\\\\''; drop table x; --'");
        assert_eq!(crate::split(PG, &b.text).len(), 1);
    }

    #[test]
    fn bind_mysql_backslashes() {
        let b = bind(
            MY,
            "select :a",
            &[pv(":a", ParamInput::Text("a\\b".into()))],
        )
        .unwrap();
        assert_eq!(b.text, "select 'a\\\\b'");
    }

    #[test]
    fn bind_rejects_bad_numbers_and_missing_values() {
        for bad in [
            "",
            "abc",
            "1e",
            "1.",
            ".5",
            "1 2",
            "1; drop table x",
            "--1",
            "+1",
            "1e+",
        ] {
            let e = bind(PG, "select :a", &[pv(":a", ParamInput::Number(bad.into()))]).unwrap_err();
            assert_eq!(e.kind(), "query", "{bad}");
        }
        for ok in ["0", "-5", "12.50", "1e10", "1E-3", "-0.5e+2"] {
            assert!(
                bind(PG, "select :a", &[pv(":a", ParamInput::Number(ok.into()))]).is_ok(),
                "{ok}"
            );
        }
        let e = bind(PG, "select :a, :b", &[pv(":a", ParamInput::Null)]).unwrap_err();
        assert!(e.message().contains(":b"));
        let e = bind(PG, "select :a", &[pv(":a", ParamInput::Text("x\0".into()))]).unwrap_err();
        assert_eq!(e.kind(), "query");
        // extra values are ignored
        assert!(bind(PG, "select 1", &[pv(":zzz", ParamInput::Null)]).is_ok());
    }

    #[test]
    fn bind_text_cannot_break_out() {
        let b = bind(
            PG,
            "select :a",
            &[pv(":a", ParamInput::Text("'; drop table x; --".into()))],
        )
        .unwrap();
        assert_eq!(b.text, "select '''; drop table x; --'");
        assert_eq!(crate::split(PG, &b.text).len(), 1);
    }

    #[test]
    fn offset_map_round_trip() {
        let text = "select :a, 'é', :bb from t where x = :a";
        let b = bind(
            PG,
            text,
            &[
                pv(":a", ParamInput::Text("long value".into())),
                pv(":bb", ParamInput::Null),
            ],
        )
        .unwrap();
        assert_eq!(
            b.text,
            "select 'long value', 'é', NULL from t where x = 'long value'"
        );
        let bc: Vec<char> = b.text.chars().collect();
        let oc: Vec<char> = text.chars().collect();
        // Positions of untouched characters map to the same character.
        for (i, ch) in bc.iter().enumerate() {
            let inside = b
                .map
                .edits
                .iter()
                .any(|e| (e.bound_start as usize..e.bound_end as usize).contains(&i));
            if !inside {
                let o = b.map.to_original(to_u32(i)) as usize;
                assert_eq!(oc[o], *ch, "bound {i} -> orig {o}");
            }
        }
        let bp = b.text.chars().position(|c| c == 'f').unwrap();
        let op = oc.iter().position(|c| *c == 'f').unwrap();
        assert_eq!(b.map.to_original(to_u32(bp)) as usize, op);
        // Inside a replaced region -> start of the placeholder.
        assert_eq!(b.map.to_original(9), 7);
        // The end maps to the end.
        assert_eq!(b.map.to_original(to_u32(bc.len())) as usize, oc.len());
    }

    #[test]
    fn offset_map_maps_back_across_replacements() {
        let map = OffsetMap {
            edits: vec![Edit {
                bound_start: 2,
                bound_end: 9,
                orig_start: 2,
                orig_end: 5,
            }],
        };
        assert_eq!(map.to_original(1), 1);
        assert_eq!(map.to_original(4), 2);
        assert_eq!(map.to_original(9), 5);
        assert_eq!(map.to_original(10), 6);
    }

    #[test]
    fn find_params_keeps_unicode_offsets() {
        let p = find_params(PG, "select 'é😀', :x");
        assert_eq!(p[0].ranges[0], TextRange { start: 13, end: 15 });
    }
}
