//! Query plan commands (package E): EXPLAIN, parsing and `.dbplan.json` files.

use std::path::Path;

use dbstudio_core::{Error, ExplainRequest, RawPlan};
use dbstudio_plan::{PlanError, PlanRules, PlanTree};
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

use super::app::write_atomic;
use super::blocking;
use crate::grants::Purpose;
use crate::host::FileFilter;
use crate::state::AppState;

type Result<T> = std::result::Result<T, Error>;

/// Result of `plan_explain`: the raw plan always, the tree when it could be parsed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanResult {
    pub raw: RawPlan,
    pub tree: Option<PlanTree>,
    pub parse_error: Option<String>,
}

fn plan_filters() -> Vec<FileFilter> {
    vec![FileFilter {
        name: "DBStudio plan".into(),
        extensions: vec!["json".into()],
    }]
}

fn plan_error(e: PlanError) -> Error {
    match e {
        PlanError::Unsupported(format) => Error::Unsupported(format!("plan_format:{format:?}")),
        PlanError::Invalid(message) => Error::Other(message),
    }
}

/// Rules the user asked for, else the defaults with the misestimate factor from the settings.
fn effective_rules(state: &AppState, rules: Option<PlanRules>) -> PlanRules {
    rules.unwrap_or_else(|| {
        let factor = state
            .connections
            .settings()
            .map(|s| f64::from(s.plan_misestimate_factor))
            .unwrap_or_else(|_| PlanRules::default().misestimate_factor);
        PlanRules {
            misestimate_factor: factor,
            ..PlanRules::default()
        }
    })
}

pub(crate) async fn explain_impl(
    state: &AppState,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    request: ExplainRequest,
    rules: Option<PlanRules>,
) -> Result<PlanResult> {
    let raw = match lease_id {
        Some(lease_id) => state.leases.get(lease_id)?.explain(request).await?,
        None => state.sessions.get(session_id)?.explain(request).await?,
    };
    let rules = effective_rules(state, rules);
    let (tree, parse_error) = match dbstudio_plan::parse_and_analyze(&raw, &rules) {
        Ok(tree) => (Some(tree), None),
        Err(e) => (None, Some(e.to_string())),
    };
    Ok(PlanResult {
        raw,
        tree,
        parse_error,
    })
}

pub(crate) fn parse_impl(
    state: &AppState,
    raw: RawPlan,
    rules: Option<PlanRules>,
) -> Result<PlanTree> {
    dbstudio_plan::parse_and_analyze(&raw, &effective_rules(state, rules)).map_err(plan_error)
}

/// Save a plan through a Save dialog; `None` when cancelled.
pub(crate) async fn file_save_impl(state: &AppState, raw: RawPlan) -> Result<Option<String>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let Some(picked) = host.pick_save_path_filtered("plan.dbplan.json", &plan_filters())?
        else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Write)?;
        let json = serde_json::to_vec_pretty(&raw)
            .map_err(|e| Error::Other(format!("cannot serialize plan: {e}")))?;
        write_atomic(&path, &json)?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
}

/// Open a plan through an Open dialog; `None` when cancelled.
pub(crate) async fn file_open_impl(state: &AppState) -> Result<Option<RawPlan>> {
    let host = state.host.clone();
    let grants = state.grants.clone();
    blocking(move || {
        let Some(picked) = host.pick_open_path(&plan_filters(), false)? else {
            return Ok(None);
        };
        let path = grants.grant(&picked, Purpose::Read)?;
        let bytes = std::fs::read(Path::new(&path))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Other(format!("not a DBStudio plan file: {e}")))
    })
    .await
}

#[tauri::command]
pub async fn plan_explain(
    state: State<'_, AppState>,
    session_id: Uuid,
    lease_id: Option<Uuid>,
    request: ExplainRequest,
    rules: Option<PlanRules>,
) -> Result<PlanResult> {
    tracing::debug!(command = "plan_explain", session = %session_id, "ipc call");
    explain_impl(&state, session_id, lease_id, request, rules).await
}

#[tauri::command]
pub fn plan_parse(
    state: State<'_, AppState>,
    raw: RawPlan,
    rules: Option<PlanRules>,
) -> Result<PlanTree> {
    tracing::debug!(command = "plan_parse", "ipc call");
    parse_impl(&state, raw, rules)
}

#[tauri::command]
pub async fn plan_file_save(state: State<'_, AppState>, raw: RawPlan) -> Result<Option<String>> {
    tracing::debug!(command = "plan_file_save", "ipc call");
    file_save_impl(&state, raw).await
}

#[tauri::command]
pub async fn plan_file_open(state: State<'_, AppState>) -> Result<Option<RawPlan>> {
    tracing::debug!(command = "plan_file_open", "ipc call");
    file_open_impl(&state).await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dbstudio_core::{DriverRegistry, PlanFormat};
    use dbstudio_plan::PlanRule;

    use super::*;
    use crate::state::testing::test_state_with_host;

    const PG_SPILL: &str =
        include_str!("../../../crates/dbstudio-plan/tests/fixtures/pg_04_sort_spill_analyze.json");

    fn raw(format: PlanFormat, text: &str) -> RawPlan {
        RawPlan {
            format,
            text: text.into(),
            statement: "select * from items order by sku".into(),
            analyzed: true,
            rolled_back: true,
            server_version: "16.2".into(),
            captured_at: "2025-01-01T00:00:00Z".into(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn parse_returns_an_analysed_tree() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let tree = parse_impl(&state, raw(PlanFormat::PostgresJson, PG_SPILL), None).unwrap();
        assert!(tree.analyzed);
        assert!(
            tree.warnings
                .iter()
                .any(|w| w.rule == PlanRule::SpillToDisk)
        );
        assert_eq!(tree.hot_path.first(), Some(&0));
        // Explicit rules win over the defaults.
        let rules = PlanRules {
            large_table_rows: 10.0,
            ..PlanRules::default()
        };
        let strict =
            parse_impl(&state, raw(PlanFormat::PostgresJson, PG_SPILL), Some(rules)).unwrap();
        assert!(
            strict
                .warnings
                .iter()
                .any(|w| w.rule == PlanRule::FullScanLargeTable)
        );
    }

    #[test]
    fn parse_errors_are_plain_errors() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let e = parse_impl(&state, raw(PlanFormat::PostgresJson, "{}"), None).unwrap_err();
        assert!(matches!(e, Error::Other(_)), "{e:?}");
    }

    #[test]
    fn default_rules_use_the_settings_factor() {
        let (state, _, _) = test_state_with_host(DriverRegistry::new());
        let rules = effective_rules(&state, None);
        assert!(rules.misestimate_factor >= 1.0);
        assert_eq!(
            rules.large_table_rows,
            PlanRules::default().large_table_rows
        );
    }

    #[tokio::test]
    async fn plan_files_round_trip_through_dialogs() {
        let (state, _, host) = test_state_with_host(DriverRegistry::new());
        let original = raw(PlanFormat::PostgresJson, PG_SPILL);
        // Cancelled dialogs do nothing.
        assert_eq!(
            file_save_impl(&state, original.clone()).await.unwrap(),
            None
        );
        assert_eq!(file_open_impl(&state).await.unwrap(), None);

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("q.dbplan.json");
        *host.save_to.lock().unwrap() = Some(target.clone());
        let saved = file_save_impl(&state, original.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(saved.ends_with("q.dbplan.json"));
        assert_eq!(
            host.suggested.lock().unwrap().as_slice(),
            ["plan.dbplan.json", "plan.dbplan.json"]
        );

        *host.open_from.lock().unwrap() = Some(target.clone());
        let opened = file_open_impl(&state).await.unwrap().unwrap();
        assert_eq!(opened, original);

        // A file that is not a plan file is rejected, not misread.
        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, "[1, 2]").unwrap();
        *host.open_from.lock().unwrap() = Some(junk);
        assert!(file_open_impl(&state).await.is_err());
    }
}
