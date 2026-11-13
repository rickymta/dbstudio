//! Export / import against PostgreSQL 16 and 12 (only with `DBSTUDIO_IT=1`).

mod common;
mod it_support;

use it_support::{Env, enabled};

async fn all_scenarios(env: Env) {
    it_support::sql_round_trip(&env).await;
    it_support::xlsx_split(&env).await;
    it_support::csv_and_json_files(&env).await;
    it_support::query_export(&env).await;
    it_support::csv_round_trip(&env).await;
    it_support::import_conflicts(&env).await;
    it_support::csv_import(&env, 20_000).await;
    it_support::cross_database(&env).await;
    it_support::lease_hooks(&env).await;
    it_support::single_transaction_rolls_back(&env).await;
    it_support::typed_round_trips(&env).await;
    it_support::hostile_values(&env).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres16() {
    if !enabled() {
        return;
    }
    all_scenarios(Env::postgres(54316, "pg16").await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres12() {
    if !enabled() {
        return;
    }
    all_scenarios(Env::postgres(54312, "pg12").await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres16_big() {
    if !it_support::big_enabled() {
        return;
    }
    let env = Env::postgres(54316, "pg16-big").await;
    it_support::big_export(&env).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres16_million_row_import() {
    if !enabled() {
        return;
    }
    let env = Env::postgres(54316, "pg16-million").await;
    it_support::csv_import(&env, 1_000_000).await;
}
