//! `explain`: `EXPLAIN PLAN` + `PLAN_TABLE` (estimated plan), or the statement run with row source
//! statistics and read back from `V$SQL_PLAN_STATISTICS_ALL` (`analyze`). Both give a
//! [`PlanFormat::OraclePlanRows`] plan: a JSON array with one object per plan line, keyed by the
//! view's upper-case column names, plus the `DBMS_XPLAN` text in `extra.xplan`.

use std::collections::BTreeMap;

use dbstudio_core::{Error, ExplainRequest, PlanFormat, RawPlan, Result, Value};
use oracle::sql_type::OracleType;
use oracle::{Connection, Row};

use crate::catalog::{self, cell, rows};
use crate::error::map_error;
use crate::exec::read_only_violation;
use crate::script::{self, StmtKind};
use crate::value::{decode, plan_column};

/// Statement ids are at most 30 characters.
pub fn statement_id(query_id: uuid::Uuid) -> String {
    let hex = query_id.simple().to_string();
    format!("DBS{}", &hex[..24])
}

/// The single statement of `text` (an explain applies to exactly one).
fn single_statement(text: &str) -> Result<script::Stmt> {
    let mut stmts = script::split(text);
    match stmts.len() {
        0 => Err(Error::query("there is no statement to explain")),
        1 => Ok(stmts.remove(0)),
        _ => Err(Error::query(
            "explain works on one statement: select it and try again",
        )),
    }
}

/// JSON object of one result row, keyed by column name.
fn row_to_json(row: &Row, max_lob: u64) -> Result<serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (info, sv) in row.column_info().iter().zip(row.sql_values()) {
        let (plan, meta) = plan_column(info);
        let json = match decode(sv, plan, max_lob)? {
            Value::Null => serde_json::Value::Null,
            Value::Int(i) => serde_json::Value::from(i),
            Value::UInt(u) => serde_json::Value::from(u),
            Value::Float(f) => serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Decimal(s) => match s.parse::<i64>() {
                Ok(i) => serde_json::Value::from(i),
                Err(_) => match s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
                    Some(n) => serde_json::Value::Number(n),
                    None => serde_json::Value::String(s),
                },
            },
            Value::Bool(b) => serde_json::Value::Bool(b),
            Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::Timestamp(s)
            | Value::Uuid(s)
            | Value::Json(s) => serde_json::Value::String(s),
            Value::Ext(e) => serde_json::Value::String(e.text),
            Value::Bytes(_) | Value::Array(_) | Value::Document(_) => serde_json::Value::Null,
        };
        map.insert(meta.name.to_ascii_uppercase(), json);
    }
    Ok(serde_json::Value::Object(map))
}

/// Run `sql` and return every row as a JSON object.
fn json_rows(
    conn: &Connection,
    sql: &str,
    params: &[&dyn oracle::sql_type::ToSql],
) -> Result<Vec<serde_json::Value>> {
    let rs = conn.query(sql, params).map_err(map_error)?;
    let mut out = Vec::new();
    for row in rs {
        let row = row.map_err(map_error)?;
        out.push(row_to_json(&row, 1_000_000)?);
    }
    Ok(out)
}

fn xplan_text(
    conn: &Connection,
    sql: &str,
    params: &[&dyn oracle::sql_type::ToSql],
) -> Result<String> {
    let lines = rows(conn, sql, params)?;
    Ok(lines
        .iter()
        .map(|r| cell(r, 0))
        .collect::<Vec<_>>()
        .join("\n"))
}

const PLAN_TABLE_SQL: &str = "SELECT id, parent_id, operation, options, object_owner, object_name, \
    object_alias, object_type, optimizer, search_columns, position, cost, cardinality, bytes, \
    other_tag, partition_start, partition_stop, partition_id, distribution, cpu_cost, io_cost, \
    temp_space, access_predicates, filter_predicates, projection, time, qblock_name \
    FROM plan_table WHERE statement_id = :1 ORDER BY id";

/// Words that start statements which cannot be analysed (they commit or are not DML/queries).
fn analyze_blocker(words: &[String], kind: &StmtKind) -> Option<&'static str> {
    if matches!(kind, StmtKind::PlSql | StmtKind::Exec) {
        return Some("explain_analyze_plsql");
    }
    match words.first().map(String::as_str) {
        Some("SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "(") => None,
        _ => Some("explain_analyze_ddl"),
    }
}

fn is_write(words: &[String]) -> bool {
    matches!(
        words.first().map(String::as_str),
        Some("INSERT" | "UPDATE" | "DELETE" | "MERGE")
    )
}

/// Where the explain runs: how the surrounding transaction is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMode {
    /// Pooled connection: nothing of the user's is open, a full rollback is fine.
    Detached,
    /// Lease with an open transaction: roll back to a savepoint only.
    InTransaction,
    /// Lease without an open transaction.
    Idle,
}

/// Produce the plan of `req` on `conn`.
pub fn explain(
    conn: &Connection,
    req: &ExplainRequest,
    read_only: bool,
    tx: TxMode,
) -> Result<RawPlan> {
    let stmt = single_statement(&req.text)?;
    let words = script::leading_words(&stmt.text, 2);
    if read_only && let Some(message) = read_only_violation(&stmt) {
        // EXPLAIN PLAN itself is allowed; only the analysed execution of a write is not.
        if req.analyze {
            return Err(Error::ReadOnlyViolation {
                message,
                position: Some(u32::try_from(stmt.start).unwrap_or(u32::MAX)),
            });
        }
    }
    let version = conn
        .server_version()
        .map(|(v, _)| v.to_string())
        .unwrap_or_default();
    let captured_at = catalog::scalar(
        conn,
        "SELECT TO_CHAR(SYS_EXTRACT_UTC(SYSTIMESTAMP), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM dual",
        &[],
    )?
    .unwrap_or_default();
    let mut extra = BTreeMap::new();
    let (plan_rows, analyzed, rolled_back) = if req.analyze {
        if let Some(code) = analyze_blocker(&words, &stmt.kind) {
            return Err(Error::Unsupported(code.into()));
        }
        analyze(conn, req, &stmt.text, &words, tx, &mut extra)?
    } else {
        let id = statement_id(req.query_id);
        // The statement text is spliced after FOR: the splitter already removed the terminator.
        conn.execute(
            &format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR {}", stmt.text),
            &[],
        )
        .map_err(|e| {
            crate::error::map_statement_error(
                e,
                0,
                &format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR {}", stmt.text),
            )
        })?;
        let rows = json_rows(conn, PLAN_TABLE_SQL, &[&id])?;
        extra.insert(
            "xplan".to_string(),
            xplan_text(
                conn,
                "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY('PLAN_TABLE', :1, 'ALL'))",
                &[&id],
            )?,
        );
        extra.insert("statementId".to_string(), id.clone());
        let _ = conn.execute("DELETE FROM plan_table WHERE statement_id = :1", &[&id]);
        if tx == TxMode::Detached {
            let _ = conn.rollback();
        }
        (rows, false, false)
    };
    Ok(RawPlan {
        format: PlanFormat::OraclePlanRows,
        text: serde_json::Value::Array(plan_rows).to_string(),
        statement: stmt.text,
        analyzed,
        rolled_back,
        server_version: version,
        captured_at,
        extra,
    })
}

fn permission(e: Error) -> Error {
    let m = e.message();
    if m.contains("ORA-00942") || m.contains("ORA-01031") {
        Error::Permission(format!(
            "reading V$SQL and V$SQL_PLAN_STATISTICS_ALL is not allowed for this user \
             (grant SELECT_CATALOG_ROLE to get actual plans): {m}"
        ))
    } else {
        e
    }
}

/// Run the statement with row source statistics, read them back and undo the statement.
fn analyze(
    conn: &Connection,
    req: &ExplainRequest,
    text: &str,
    words: &[String],
    tx: TxMode,
    extra: &mut BTreeMap<String, String>,
) -> Result<(Vec<serde_json::Value>, bool, bool)> {
    let write = is_write(words);
    let tag = format!("dbstudio:{}", req.query_id.simple());
    let tagged = format!("/* {tag} */ {text}");
    conn.execute("ALTER SESSION SET STATISTICS_LEVEL = ALL", &[])
        .map_err(map_error)?;
    let savepoint = "DBS_EXPLAIN";
    if write && tx == TxMode::InTransaction {
        conn.execute(&format!("SAVEPOINT {savepoint}"), &[])
            .map_err(map_error)?;
    }
    let result = run_and_collect(conn, &tagged, &tag, extra);
    // Undo the statement whatever happened.
    if write {
        match tx {
            TxMode::InTransaction => {
                let _ = conn.execute(&format!("ROLLBACK TO SAVEPOINT {savepoint}"), &[]);
            }
            _ => {
                let _ = conn.rollback();
            }
        }
    }
    let _ = conn.execute("ALTER SESSION SET STATISTICS_LEVEL = TYPICAL", &[]);
    let plan_rows = result.map_err(permission)?;
    Ok((plan_rows, true, write))
}

fn run_and_collect(
    conn: &Connection,
    tagged: &str,
    tag: &str,
    extra: &mut BTreeMap<String, String>,
) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.statement(tagged).build().map_err(map_error)?;
    if stmt.is_query() {
        let rs = stmt.query(&[]).map_err(map_error)?;
        // Read everything: row source statistics are only complete after the last fetch.
        for row in rs {
            row.map_err(map_error)?;
        }
    } else {
        stmt.execute(&[]).map_err(map_error)?;
    }
    drop(stmt);
    let like = format!("%{tag}%");
    let found = rows(
        conn,
        "SELECT sql_id, child_number FROM v$sql \
         WHERE sql_text LIKE :1 AND sql_text NOT LIKE '%v$sql%' \
         ORDER BY last_active_time DESC FETCH FIRST 1 ROWS ONLY",
        &[&like],
    )?;
    let Some(first) = found.first() else {
        return Err(Error::Other(
            "the statement was not found in V$SQL (it may have been aged out)".into(),
        ));
    };
    let (sql_id, child) = (cell(first, 0), cell(first, 1));
    let child_n: i64 = child.parse().unwrap_or(0);
    let plan_rows = json_rows(
        conn,
        "SELECT id, parent_id, operation, options, object_owner, object_name, object_alias, \
                object_type, optimizer, cost, cardinality, bytes, cpu_cost, io_cost, temp_space, \
                access_predicates, filter_predicates, projection, time, qblock_name, \
                starts, output_rows, cr_buffer_gets, disk_reads, disk_writes, elapsed_time, \
                last_starts, last_output_rows, last_cr_buffer_gets, last_disk_reads, \
                last_elapsed_time, last_memory_used, last_tempseg_size \
         FROM v$sql_plan_statistics_all WHERE sql_id = :1 AND child_number = :2 ORDER BY id",
        &[&sql_id, &child_n],
    )?;
    extra.insert(
        "xplan".to_string(),
        xplan_text(
            conn,
            "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(:1, :2, 'ALLSTATS LAST'))",
            &[&sql_id, &child_n],
        )?,
    );
    extra.insert("sqlId".to_string(), sql_id);
    extra.insert("childNumber".to_string(), child);
    Ok(plan_rows)
}

/// Re-exported so the unused-import lint stays quiet when OracleType is only used in tests.
#[allow(dead_code)]
fn _type_marker(_: OracleType) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_ids_fit_in_30_characters() {
        let id = statement_id(uuid::Uuid::new_v4());
        assert_eq!(id.len(), 27);
        assert!(id.starts_with("DBS"));
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn only_one_statement_can_be_explained() {
        assert!(single_statement("select 1 from dual").is_ok());
        assert!(single_statement("select 1 from dual; select 2 from dual").is_err());
        assert!(single_statement("  ").is_err());
    }

    #[test]
    fn analyze_is_limited_to_queries_and_dml() {
        let w = |s: &str| script::leading_words(s, 2);
        let sql = StmtKind::Sql;
        assert!(analyze_blocker(&w("select 1 from dual"), &sql).is_none());
        assert!(analyze_blocker(&w("update t set a = 1"), &sql).is_none());
        assert!(analyze_blocker(&w("create table t (a number)"), &sql).is_some());
        assert_eq!(
            analyze_blocker(&w("begin null; end;"), &StmtKind::PlSql),
            Some("explain_analyze_plsql")
        );
        assert!(is_write(&w("delete from t")));
        assert!(!is_write(&w("select 1 from dual")));
    }
}
