//! Export / import commands and export presets (package G).

use std::path::Path;
use std::sync::Arc;

use dbstudio_core::{
    ConfirmationGrant, ConfirmationRequest, ConnectionConfig, DestructiveStatement, Error, JobId,
    JobKind, JobReporter, OnConflict, ScriptDialect, Session, TextRange,
};
use dbstudio_export::{
    ExportContext, ExportSpec, ImportFormat, ImportPreview, ImportPreviewRequest, ImportSpec,
};
use dbstudio_store::ExportPreset;
use tauri::State;
use uuid::Uuid;

use super::blocking;
use crate::grants::Purpose;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

/// Session a job runs on, plus the profile settings of its connection.
///
/// A job owns its session (it must outlive the tab that started it), opened from the saved
/// profile. Proposal: look up an already open session of the connection first, so that
/// profiles whose password is not stored work too.
struct JobSession {
    session_id: Uuid,
    session: Arc<dyn Session>,
    config: ConnectionConfig,
}

async fn open_job_session(state: &AppState, connection_id: Uuid) -> Result<JobSession> {
    let svc = state.connections.clone();
    let (profile, secret) = blocking(move || svc.session_credentials(connection_id, None)).await?;
    let info = state
        .sessions
        .open(&profile.config, secret.as_ref())
        .await?;
    let session = state.sessions.get(info.session_id)?;
    Ok(JobSession {
        session_id: info.session_id,
        session,
        config: profile.config,
    })
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn export_result(report: &dbstudio_export::ExportReport) -> serde_json::Value {
    serde_json::json!({
        "path": report.files.first(),
        "files": report.files,
        "sizeBytes": report.bytes,
        "rows": report.rows,
        "objects": report.objects,
        "warnings": report.warnings,
    })
}

pub(crate) async fn start_impl(state: &AppState, spec: ExportSpec) -> Result<JobId> {
    // Only a path the user picked in a Save dialog (or its `.zip` sibling, used when several
    // files are produced) may be written.
    let dest = Path::new(&spec.destination.path);
    state.grants.check(dest, Purpose::Write)?;
    state
        .grants
        .grant(&dest.with_extension("zip"), Purpose::Write)?;

    let job = open_job_session(state, spec.connection_id).await?;
    let title = format!(
        "Export {} → {}",
        job.config.name,
        file_name(&spec.destination.path)
    );
    let connection_id = spec.connection_id;
    let sessions = Arc::clone(&state.sessions);
    Ok(state.jobs.spawn(
        JobKind::Export,
        title,
        Some(connection_id),
        move |ctx| async move {
            let reporter: Arc<dyn JobReporter> = ctx;
            let result = dbstudio_export::run_export(
                ExportContext {
                    dialect: job.session.script_dialect(),
                    kind: job.config.kind,
                    session: job.session,
                    reporter,
                },
                spec,
            )
            .await;
            let _ = sessions.close(job.session_id).await;
            result.map(|r| Some(export_result(&r)))
        },
    ))
}

pub(crate) async fn presets_list_impl(
    state: &AppState,
    connection_id: Option<Uuid>,
) -> Result<Vec<ExportPreset>> {
    let svc = state.connections.clone();
    blocking(move || svc.store().presets_list(connection_id)).await
}

pub(crate) async fn preset_save_impl(
    state: &AppState,
    preset: ExportPreset,
) -> Result<ExportPreset> {
    let svc = state.connections.clone();
    blocking(move || svc.store().presets_save(&preset)).await
}

pub(crate) async fn preset_delete_impl(state: &AppState, id: Uuid) -> Result<()> {
    let svc = state.connections.clone();
    blocking(move || svc.store().presets_delete(id)).await
}

/// Only files the user picked in a dialog can be previewed.
pub(crate) async fn import_preview_impl(
    state: &AppState,
    request: ImportPreviewRequest,
) -> Result<ImportPreview> {
    state
        .grants
        .check(Path::new(&request.file), Purpose::Read)?;
    dbstudio_export::preview_import(request).await
}

/// Destructive statements of a SQL file (stops after [`MAX_LISTED`]).
const MAX_LISTED: usize = 50;

fn scan_script(path: &Path, dialect: ScriptDialect) -> Result<Vec<DestructiveStatement>> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?;
    let mut reader = std::io::BufReader::new(file);
    let mut splitter = dbstudio_sql::StreamSplitter::new(dialect);
    let mut found = Vec::new();
    let (mut index, mut offset) = (0u32, 0u32);
    let mut line = String::new();
    let mut eof = false;
    let mut visit = |stmt: String, found: &mut Vec<DestructiveStatement>| {
        let len = u32::try_from(stmt.chars().count()).unwrap_or(u32::MAX);
        if let Some(info) = dbstudio_sql::analyze(dialect, &stmt).statements.first() {
            if let Some(reason) = info.destructive {
                found.push(DestructiveStatement {
                    index,
                    range: TextRange {
                        start: offset,
                        end: offset.saturating_add(len),
                    },
                    text: stmt.chars().take(2000).collect(),
                    reason,
                });
            }
        }
        index += 1;
        offset = offset.saturating_add(len);
    };
    while !eof && found.len() < MAX_LISTED {
        line.clear();
        let n = std::io::BufRead::read_line(&mut reader, &mut line)
            .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?;
        let statements = if n == 0 {
            eof = true;
            splitter.clone().finish()
        } else {
            splitter.push(&line)
        };
        for stmt in statements {
            visit(stmt, &mut found);
        }
    }
    Ok(found)
}

/// Production connections ask for confirmation before a script with destructive statements runs
/// or rows are overwritten.
fn confirm_import(
    state: &AppState,
    config: &ConnectionConfig,
    dialect: ScriptDialect,
    spec: &ImportSpec,
    file: &Path,
    confirm: Option<&ConfirmationGrant>,
) -> Result<()> {
    let (_, production) = crate::guard::policy_flags(config);
    if !production {
        return Ok(());
    }
    let statements = match spec.format {
        ImportFormat::Sql { .. } => scan_script(file, dialect)?,
        _ => Vec::new(),
    };
    let overwrite = spec.on_conflict == Some(OnConflict::Update);
    if statements.is_empty() && !overwrite {
        return Ok(());
    }
    let meta = std::fs::metadata(file).ok();
    let payload = format!(
        "{}\0{}\0{:?}\0{overwrite}",
        file.display(),
        meta.as_ref().map_or(0, std::fs::Metadata::len),
        meta.and_then(|m| m.modified().ok()),
    );
    let target = format!("import:{}", spec.connection_id);
    if let Some(grant) = confirm {
        if state
            .confirm
            .consume(grant, &target, payload.as_bytes())
            .is_ok()
        {
            return Ok(());
        }
    }
    let text = crate::guard::confirm_text_for(config, None);
    let (token, expires_in_ms) = state.confirm.issue(&target, payload.as_bytes(), &text);
    Err(Error::ConfirmationRequired(Box::new(ConfirmationRequest {
        token,
        confirm_text: text,
        statements,
        expires_in_ms,
    })))
}

pub(crate) async fn import_start_impl(
    state: &AppState,
    spec: ImportSpec,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    let file = state.grants.check(Path::new(&spec.file), Purpose::Read)?;
    let job = open_job_session(state, spec.connection_id).await?;
    let dialect = job.session.script_dialect();
    let checked = crate::guard::ensure_writable(&job.config, "import").and_then(|()| {
        let (state, config, spec, file) = (state, &job.config, &spec, &file);
        confirm_import(state, config, dialect, spec, file, confirm.as_ref())
    });
    if let Err(e) = checked {
        let _ = state.sessions.close(job.session_id).await;
        return Err(e);
    }
    let title = format!("Import {} → {}", file_name(&spec.file), job.config.name);
    let connection_id = spec.connection_id;
    let sessions = Arc::clone(&state.sessions);
    Ok(state.jobs.spawn(
        JobKind::Import,
        title,
        Some(connection_id),
        move |ctx| async move {
            let reporter: Arc<dyn JobReporter> = ctx;
            let result = dbstudio_export::run_import(
                ExportContext {
                    dialect,
                    kind: job.config.kind,
                    session: job.session,
                    reporter,
                },
                spec,
            )
            .await;
            let _ = sessions.close(job.session_id).await;
            result.and_then(|r| {
                serde_json::to_value(&r)
                    .map(Some)
                    .map_err(|e| Error::Other(e.to_string()))
            })
        },
    ))
}

#[tauri::command]
pub async fn export_start(state: State<'_, AppState>, spec: ExportSpec) -> Result<JobId> {
    tracing::debug!(command = "export_start", "ipc call");
    start_impl(&state, spec).await
}

#[tauri::command]
pub async fn export_presets_list(
    state: State<'_, AppState>,
    connection_id: Option<Uuid>,
) -> Result<Vec<ExportPreset>> {
    tracing::debug!(command = "export_presets_list", "ipc call");
    presets_list_impl(&state, connection_id).await
}

#[tauri::command]
pub async fn export_preset_save(
    state: State<'_, AppState>,
    preset: ExportPreset,
) -> Result<ExportPreset> {
    tracing::debug!(command = "export_preset_save", "ipc call");
    preset_save_impl(&state, preset).await
}

#[tauri::command]
pub async fn export_preset_delete(state: State<'_, AppState>, id: Uuid) -> Result<()> {
    tracing::debug!(command = "export_preset_delete", "ipc call");
    preset_delete_impl(&state, id).await
}

#[tauri::command]
pub async fn import_preview(
    state: State<'_, AppState>,
    request: ImportPreviewRequest,
) -> Result<ImportPreview> {
    tracing::debug!(command = "import_preview", "ipc call");
    import_preview_impl(&state, request).await
}

#[tauri::command]
pub async fn import_start(
    state: State<'_, AppState>,
    spec: ImportSpec,
    confirm: Option<ConfirmationGrant>,
) -> Result<JobId> {
    tracing::debug!(command = "import_start", "ipc call");
    import_start_impl(&state, spec, confirm).await
}

#[cfg(test)]
mod tests {
    use dbstudio_core::{
        ConnectionConfig, DbKind, Environment, ObjectKind, ObjectPath, ScriptDialect,
    };
    use dbstudio_export::{
        CsvOptions, ExportContent, ExportDestination, ExportFormat, ExportSource, ImportMode,
        ImportTarget, JsonLayout, TextEncoding, ValueFormat,
    };

    use super::*;
    use crate::session::testing::MockDriver;
    use crate::state::testing::test_state;

    fn state() -> AppState {
        let (driver, _) = MockDriver::new(DbKind::Postgres);
        let mut registry = dbstudio_core::DriverRegistry::new();
        registry.register(driver);
        test_state(registry).0
    }

    fn export_spec(path: &str) -> ExportSpec {
        ExportSpec {
            connection_id: Uuid::new_v4(),
            source: ExportSource::Query {
                text: "SELECT 1".into(),
                database: None,
            },
            content: ExportContent::Data,
            format: ExportFormat::Csv(CsvOptions::default()),
            destination: ExportDestination {
                path: path.into(),
                overwrite: false,
            },
            values: ValueFormat::default(),
        }
    }

    fn import_spec(file: &str, format: ImportFormat) -> ImportSpec {
        ImportSpec {
            connection_id: Uuid::new_v4(),
            file: file.into(),
            format,
            target: ImportTarget::Script { database: None },
            mapping: vec![],
            on_conflict: None,
            mode: ImportMode::Batches { rows: 10 },
            max_errors: 0,
        }
    }

    #[tokio::test]
    async fn export_only_writes_granted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        let err = start_impl(&state(), export_spec(path.to_str().unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Permission(_)), "{err:?}");
        // A read grant is not a write grant.
        let s = state();
        s.grants.grant(&path, Purpose::Read).unwrap();
        assert!(matches!(
            start_impl(&s, export_spec(path.to_str().unwrap()))
                .await
                .unwrap_err(),
            Error::Permission(_)
        ));
        assert!(s.jobs.list().is_empty());
    }

    #[tokio::test]
    async fn import_only_reads_granted_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("in.csv");
        std::fs::write(&file, "a\n1\n").unwrap();
        let s = state();
        let spec = import_spec(
            file.to_str().unwrap(),
            ImportFormat::Json {
                layout: JsonLayout::Array,
            },
        );
        assert!(matches!(
            import_start_impl(&s, spec.clone(), None).await.unwrap_err(),
            Error::Permission(_)
        ));
        let req = ImportPreviewRequest {
            file: file.to_string_lossy().into_owned(),
            format: ImportFormat::Csv {
                delimiter: ',',
                quote: '"',
                header: true,
                encoding: TextEncoding::Utf8,
            },
            rows: 10,
        };
        assert!(matches!(
            import_preview_impl(&s, req.clone()).await.unwrap_err(),
            Error::Permission(_)
        ));
        s.grants.grant(&file, Purpose::Read).unwrap();
        let preview = import_preview_impl(&s, req).await.unwrap();
        assert_eq!(preview.columns, ["a"]);
    }

    fn config(production: bool) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(DbKind::Postgres, "prod", "h");
        c.database = Some("shop".into());
        if production {
            c.environment = Some(Environment::Production);
        }
        c
    }

    #[test]
    fn destructive_scripts_into_production_need_a_typed_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("d.sql");
        std::fs::write(&file, "INSERT INTO t VALUES (1);\nDROP TABLE old;\n").unwrap();
        let s = state();
        let spec = import_spec(
            file.to_str().unwrap(),
            ImportFormat::Sql {
                continue_on_error: false,
            },
        );
        let dialect = ScriptDialect::Postgres;
        // Not production: no question.
        confirm_import(&s, &config(false), dialect, &spec, &file, None).unwrap();
        // Production: the destructive statement is listed and a token is issued.
        let Err(Error::ConfirmationRequired(req)) =
            confirm_import(&s, &config(true), dialect, &spec, &file, None)
        else {
            panic!("expected a confirmation");
        };
        assert_eq!(req.confirm_text, "shop");
        assert_eq!(req.statements.len(), 1);
        assert_eq!(req.statements[0].index, 1);
        assert!(req.statements[0].text.starts_with("DROP TABLE old"));
        // Wrong text: asked again (the token is single use).
        let wrong = ConfirmationGrant {
            token: req.token.clone(),
            typed: "nope".into(),
        };
        assert!(matches!(
            confirm_import(&s, &config(true), dialect, &spec, &file, Some(&wrong)),
            Err(Error::ConfirmationRequired(_))
        ));
        let Err(Error::ConfirmationRequired(req)) =
            confirm_import(&s, &config(true), dialect, &spec, &file, None)
        else {
            panic!("expected a confirmation");
        };
        let grant = ConfirmationGrant {
            token: req.token,
            typed: "shop".into(),
        };
        confirm_import(&s, &config(true), dialect, &spec, &file, Some(&grant)).unwrap();
        // The token is gone, and the file changing would also invalidate it.
        assert!(confirm_import(&s, &config(true), dialect, &spec, &file, Some(&grant)).is_err());
    }

    #[test]
    fn harmless_scripts_and_overwrites_on_production() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("h.sql");
        std::fs::write(&file, "INSERT INTO t VALUES (1);\n").unwrap();
        let s = state();
        let dialect = ScriptDialect::Postgres;
        let sql = import_spec(
            file.to_str().unwrap(),
            ImportFormat::Sql {
                continue_on_error: false,
            },
        );
        confirm_import(&s, &config(true), dialect, &sql, &file, None).unwrap();
        // Overwriting rows of a production table is destructive even without statements.
        let mut table = import_spec(
            file.to_str().unwrap(),
            ImportFormat::Csv {
                delimiter: ',',
                quote: '"',
                header: true,
                encoding: TextEncoding::Utf8,
            },
        );
        table.target = ImportTarget::Existing {
            table: ObjectPath::root()
                .child(ObjectKind::Database, "shop")
                .child(ObjectKind::Table, "t"),
        };
        confirm_import(&s, &config(true), dialect, &table, &file, None).unwrap();
        table.on_conflict = Some(OnConflict::Update);
        let Err(Error::ConfirmationRequired(req)) =
            confirm_import(&s, &config(true), dialect, &table, &file, None)
        else {
            panic!("expected a confirmation");
        };
        assert!(req.statements.is_empty());
    }

    #[tokio::test]
    async fn presets_round_trip_through_the_commands() {
        let s = state();
        let conn = ConnectionConfig::new(DbKind::Postgres, "c", "h");
        let profile = dbstudio_store::ConnectionProfile {
            config: conn.clone(),
            group_id: None,
            color: None,
            save_password: false,
            has_password: false,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let svc = s.connections.clone();
        let id = profile.config.id;
        blocking(move || svc.save(profile, dbstudio_store::SecretChange::Clear))
            .await
            .unwrap();
        let preset = ExportPreset {
            id: Uuid::new_v4(),
            name: "mine".into(),
            connection_id: Some(id),
            spec: serde_json::json!({"format": {"kind": "csv"}}),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let saved = preset_save_impl(&s, preset).await.unwrap();
        assert_eq!(
            presets_list_impl(&s, Some(id)).await.unwrap()[0].name,
            "mine"
        );
        preset_delete_impl(&s, saved.id).await.unwrap();
        assert!(presets_list_impl(&s, Some(id)).await.unwrap().is_empty());
    }
}
