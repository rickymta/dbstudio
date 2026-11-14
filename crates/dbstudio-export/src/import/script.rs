//! Import of a SQL script: statements are cut out of the file as it is read (so a multi-gigabyte
//! dump never sits in memory) and run one by one.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::Arc;

use dbstudio_core::{
    CancelToken, ColumnMeta, EndOutcome, Error, JobProgress, QueryRequest, Result, ResultSink,
    StatementEnd, Value,
};
use dbstudio_sql::StreamSplitter;

use crate::conn::Conn;
use crate::{ExportContext, ImportReport, ImportSpec, ImportTarget, RowError};

/// Row errors kept in the report when the script is allowed to continue.
const MAX_REPORTED_ERRORS: usize = 1000;

/// Counts the rows a statement affected.
#[derive(Default)]
struct AffectedSink {
    affected: u64,
}

impl ResultSink for AffectedSink {
    fn on_columns(&mut self, _columns: Vec<ColumnMeta>) -> Result<()> {
        Ok(())
    }
    fn on_rows(&mut self, _rows: Vec<Vec<Value>>) -> Result<()> {
        Ok(())
    }
    fn on_message(&mut self, _message: String) -> Result<()> {
        Ok(())
    }
    fn on_statement_end(&mut self, end: StatementEnd) -> Result<()> {
        if let EndOutcome::Affected { rows } = end.outcome {
            self.affected += rows;
        }
        Ok(())
    }
}

/// Whether a statement starts with `INSERT` (leading comments skipped).
fn starts_with_insert(stmt: &str) -> bool {
    let mut rest = stmt.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, tail)| tail).trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, tail)| tail).trim_start();
        } else {
            break;
        }
    }
    rest.get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("insert"))
}

pub(crate) async fn run(
    ctx: &ExportContext,
    spec: &ImportSpec,
    continue_on_error: bool,
    cancel: &CancelToken,
) -> Result<ImportReport> {
    let ImportTarget::Script { database } = &spec.target else {
        return Err(Error::Invalid(
            "a SQL script is imported with the script target".into(),
        ));
    };
    let path = std::path::PathBuf::from(&spec.file);
    let file = File::open(&path)
        .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut reader = BufReader::with_capacity(256 * 1024, file);

    let (conn, _) = Conn::open(Arc::clone(&ctx.session), database.clone(), None).await?;
    let conn = Arc::new(conn);
    let result = execute_script(
        ctx,
        spec,
        continue_on_error,
        cancel,
        &conn,
        &mut reader,
        size,
    )
    .await;
    conn.close().await;
    result
}

async fn execute_script(
    ctx: &ExportContext,
    spec: &ImportSpec,
    continue_on_error: bool,
    cancel: &CancelToken,
    conn: &Arc<Conn>,
    reader: &mut BufReader<File>,
    size: u64,
) -> Result<ImportReport> {
    let mut splitter = StreamSplitter::new(ctx.dialect);
    let mut report = ImportReport::default();
    let mut bytes = 0u64;
    let mut line = String::new();
    let mut progress = JobProgress {
        phase: "script".into(),
        bytes_total: Some(size),
        ..JobProgress::default()
    };
    let mut eof = false;
    while !eof {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| Error::Other(format!("cannot read the script: {e}")))?;
        bytes += n as u64;
        let statements = if n == 0 {
            eof = true;
            splitter.clone().finish()
        } else {
            splitter.push(&line)
        };
        for stmt in statements {
            if stmt.trim().is_empty() {
                continue;
            }
            report.rows_read += 1;
            let req = QueryRequest::new(stmt.clone());
            let mut sink = AffectedSink::default();
            match conn.execute_cancellable(cancel, req, &mut sink).await {
                Ok(_) => {
                    if starts_with_insert(&stmt) {
                        report.inserted += sink.affected;
                    }
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    let message = e.message();
                    if !continue_on_error {
                        return Err(Error::Query {
                            message: format!("statement {}: {message}", report.rows_read),
                            position: None,
                        });
                    }
                    if report.errors.len() < MAX_REPORTED_ERRORS {
                        report.errors.push(RowError {
                            line: report.rows_read,
                            message,
                        });
                    }
                    if spec.max_errors > 0 && report.errors.len() as u32 > spec.max_errors {
                        return Err(Error::Query {
                            message: format!(
                                "too many errors; the last one in statement {}",
                                report.rows_read
                            ),
                            position: None,
                        });
                    }
                }
            }
            if report.rows_read % 50 == 0 {
                progress.rows_done = report.rows_read;
                progress.bytes_done = bytes;
                ctx.reporter.progress(&progress);
            }
        }
    }
    progress.rows_done = report.rows_read;
    progress.bytes_done = bytes;
    ctx.reporter.progress(&progress);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_detection() {
        assert!(starts_with_insert("INSERT INTO t VALUES (1)"));
        assert!(starts_with_insert("  insert into t values (1)"));
        assert!(starts_with_insert(
            "-- note\n/* c */ Insert into t values (1)"
        ));
        assert!(!starts_with_insert("CREATE TABLE t (a int)"));
        assert!(!starts_with_insert("ins"));
    }
}
