//! Schema snapshot read from `system_schema` (and the virtual schema), cached for a minute.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbstudio_core::{Error, Result};
use scylla::client::session::Session;
use scylla::value::CqlValue;

use crate::ident::quote_text;
use crate::rows::{Row, query_rows};
use crate::types::CqlType;

const CACHE_TTL: Duration = Duration::from_secs(60);

/// Table options copied into generated DDL (present in both Cassandra and Scylla).
const TABLE_OPTIONS: &[&str] = &[
    "bloom_filter_fp_chance",
    "caching",
    "comment",
    "compaction",
    "compression",
    "crc_check_chance",
    "default_time_to_live",
    "gc_grace_seconds",
    "max_index_interval",
    "memtable_flush_period_in_ms",
    "min_index_interval",
    "speculative_retry",
];

/// Role of a column in the primary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColKind {
    Partition,
    Clustering,
    Static,
    Regular,
}

#[derive(Debug, Clone)]
pub(crate) struct ColumnInfo {
    pub name: String,
    pub kind: ColKind,
    pub position: i64,
    pub descending: bool,
    pub ty: CqlType,
}

impl ColumnInfo {
    pub(crate) fn is_key(&self) -> bool {
        matches!(self.kind, ColKind::Partition | ColKind::Clustering)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct IndexInfo {
    pub name: String,
    pub kind: String,
    pub target: String,
    pub class: Option<String>,
    pub options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TableInfo {
    pub name: String,
    /// Partition keys, clustering keys, then static and regular columns by name.
    pub columns: Vec<ColumnInfo>,
    pub comment: Option<String>,
    /// `(name, CQL literal)` of the options worth keeping in a DDL.
    pub options: Vec<(String, String)>,
    pub indexes: Vec<IndexInfo>,
    pub triggers: Vec<(String, String)>,
    pub is_virtual: bool,
    pub is_view: bool,
    pub base_table: Option<String>,
    pub where_clause: Option<String>,
    pub include_all_columns: bool,
}

impl TableInfo {
    pub(crate) fn partition_keys(&self) -> Vec<&ColumnInfo> {
        self.columns
            .iter()
            .filter(|c| c.kind == ColKind::Partition)
            .collect()
    }

    pub(crate) fn clustering_keys(&self) -> Vec<&ColumnInfo> {
        self.columns
            .iter()
            .filter(|c| c.kind == ColKind::Clustering)
            .collect()
    }

    /// Partition then clustering columns.
    pub(crate) fn key_columns(&self) -> Vec<&ColumnInfo> {
        let mut v = self.partition_keys();
        v.extend(self.clustering_keys());
        v
    }

    pub(crate) fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub(crate) fn is_counter(&self) -> bool {
        self.columns
            .iter()
            .any(|c| matches!(c.ty.unfrozen(), CqlType::Counter))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UdtInfo {
    pub name: String,
    pub fields: Vec<(String, CqlType)>,
}

#[derive(Debug, Clone)]
pub(crate) struct FunctionInfo {
    pub name: String,
    pub arg_names: Vec<String>,
    pub arg_types: Vec<String>,
    pub return_type: String,
    pub language: String,
    pub body: String,
    pub called_on_null: bool,
}

impl FunctionInfo {
    /// `name(int, text)` — the unique display name used in the tree.
    pub(crate) fn signature(&self) -> String {
        format!("{}({})", self.name, self.arg_types.join(", "))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AggregateInfo {
    pub name: String,
    pub arg_types: Vec<String>,
    pub state_func: String,
    pub state_type: String,
    pub final_func: Option<String>,
    pub initcond: Option<String>,
    pub return_type: String,
}

impl AggregateInfo {
    pub(crate) fn signature(&self) -> String {
        format!("{}({})", self.name, self.arg_types.join(", "))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct KeyspaceInfo {
    pub name: String,
    pub durable_writes: bool,
    pub replication: BTreeMap<String, String>,
    /// ScyllaDB: tablets enabled for this keyspace (`None` when unknown / Cassandra).
    pub tablets: Option<bool>,
    pub is_virtual: bool,
    pub tables: BTreeMap<String, TableInfo>,
    pub views: BTreeMap<String, TableInfo>,
    pub types: BTreeMap<String, UdtInfo>,
    pub functions: Vec<FunctionInfo>,
    pub aggregates: Vec<AggregateInfo>,
}

impl KeyspaceInfo {
    pub(crate) fn is_system(&self) -> bool {
        is_system_keyspace(&self.name)
    }

    /// Table or view named `name`.
    pub(crate) fn relation(&self, name: &str) -> Option<&TableInfo> {
        self.tables.get(name).or_else(|| self.views.get(name))
    }

    pub(crate) fn udt_defs(&self) -> BTreeMap<String, Vec<(String, CqlType)>> {
        self.types
            .iter()
            .map(|(n, u)| (n.clone(), u.fields.clone()))
            .collect()
    }
}

/// Keyspaces owned by the server.
pub(crate) fn is_system_keyspace(name: &str) -> bool {
    name == "system"
        || name.starts_with("system_")
        || name == "dse_system"
        || name == "system_traces"
}

#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    pub keyspaces: BTreeMap<String, KeyspaceInfo>,
}

impl Snapshot {
    pub(crate) fn keyspace(&self, name: &str) -> Result<&KeyspaceInfo> {
        self.keyspaces
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("keyspace {name}")))
    }

    pub(crate) fn relation(&self, ks: &str, name: &str) -> Result<&TableInfo> {
        self.keyspace(ks)?
            .relation(name)
            .ok_or_else(|| Error::NotFound(format!("table {ks}.{name}")))
    }
}

/// Render the value of a table option as a CQL literal.
fn option_literal(v: &CqlValue) -> Option<String> {
    Some(match v {
        CqlValue::Text(s) | CqlValue::Ascii(s) => quote_text(s),
        CqlValue::Int(i) => i.to_string(),
        CqlValue::BigInt(i) => i.to_string(),
        CqlValue::Double(f) => format!("{f:?}"),
        CqlValue::Float(f) => format!("{f:?}"),
        CqlValue::Boolean(b) => b.to_string(),
        CqlValue::Map(pairs) => {
            let mut parts = Vec::new();
            for (k, val) in pairs {
                let (CqlValue::Text(k) | CqlValue::Ascii(k)) = k else {
                    return None;
                };
                let val = match val {
                    CqlValue::Text(s) | CqlValue::Ascii(s) => quote_text(s),
                    other => option_literal(other)?,
                };
                parts.push(format!("{}: {val}", quote_text(k)));
            }
            format!("{{{}}}", parts.join(", "))
        }
        _ => return None,
    })
}

fn col_kind(s: &str) -> ColKind {
    match s {
        "partition_key" => ColKind::Partition,
        "clustering" => ColKind::Clustering,
        "static" => ColKind::Static,
        _ => ColKind::Regular,
    }
}

/// Query that may fail on servers without the table: `None` then.
async fn try_rows(session: &Session, cql: &str) -> Option<Vec<Row>> {
    query_rows(session, cql).await.ok()
}

fn build_columns(
    rows: &[Row],
    udts: &BTreeMap<String, Vec<(String, CqlType)>>,
) -> BTreeMap<(String, String), Vec<ColumnInfo>> {
    let mut by_table: BTreeMap<(String, String), Vec<ColumnInfo>> = BTreeMap::new();
    for r in rows {
        let ty = CqlType::parse(&r.string("type")).resolved(udts);
        by_table
            .entry((r.string("keyspace_name"), r.string("table_name")))
            .or_default()
            .push(ColumnInfo {
                name: r.string("column_name"),
                kind: col_kind(&r.string("kind")),
                position: r.int("position").unwrap_or(-1),
                descending: r.string("clustering_order").eq_ignore_ascii_case("desc"),
                ty,
            });
    }
    for cols in by_table.values_mut() {
        cols.sort_by(|a, b| {
            let rank = |k: ColKind| match k {
                ColKind::Partition => 0,
                ColKind::Clustering => 1,
                ColKind::Static => 2,
                ColKind::Regular => 3,
            };
            rank(a.kind)
                .cmp(&rank(b.kind))
                .then(if a.is_key() {
                    a.position.cmp(&b.position)
                } else {
                    std::cmp::Ordering::Equal
                })
                .then_with(|| a.name.cmp(&b.name))
        });
    }
    by_table
}

fn table_from_row(r: &Row, columns: Vec<ColumnInfo>, is_view: bool) -> TableInfo {
    let mut options = Vec::new();
    for key in TABLE_OPTIONS {
        if let Some(lit) = r.get(key).and_then(option_literal) {
            options.push((key.to_string(), lit));
        }
    }
    TableInfo {
        name: if is_view {
            r.string("view_name")
        } else {
            r.string("table_name")
        },
        columns,
        comment: r.str("comment").filter(|c| !c.is_empty()),
        options,
        is_view,
        base_table: r.str("base_table_name"),
        where_clause: r.str("where_clause"),
        include_all_columns: r.bool("include_all_columns").unwrap_or(false),
        ..TableInfo::default()
    }
}

/// Read everything from the server.
pub(crate) async fn load(session: &Session) -> Result<Snapshot> {
    let ks_rows = query_rows(session, "SELECT * FROM system_schema.keyspaces").await?;
    let mut keyspaces: BTreeMap<String, KeyspaceInfo> = BTreeMap::new();
    for r in &ks_rows {
        let name = r.string("keyspace_name");
        keyspaces.insert(
            name.clone(),
            KeyspaceInfo {
                name,
                durable_writes: r.bool("durable_writes").unwrap_or(true),
                replication: r.text_map("replication"),
                ..KeyspaceInfo::default()
            },
        );
    }
    // ScyllaDB: tablets (`initial_tablets` is null when disabled).
    if let Some(rows) = try_rows(session, "SELECT * FROM system_schema.scylla_keyspaces").await {
        for r in rows {
            if let Some(k) = keyspaces.get_mut(&r.string("keyspace_name")) {
                k.tablets = Some(r.get("initial_tablets").is_some());
            }
        }
        for k in keyspaces.values_mut() {
            if k.tablets.is_none() {
                k.tablets = Some(false);
            }
        }
    }
    // User-defined types first (column types refer to them).
    for r in query_rows(session, "SELECT * FROM system_schema.types").await? {
        let names = r.text_list("field_names");
        let types = r.text_list("field_types");
        let ks = r.string("keyspace_name");
        if let Some(k) = keyspaces.get_mut(&ks) {
            let name = r.string("type_name");
            k.types.insert(
                name.clone(),
                UdtInfo {
                    name,
                    fields: names
                        .into_iter()
                        .zip(types)
                        .map(|(n, t)| (n, CqlType::parse(&t)))
                        .collect(),
                },
            );
        }
    }
    // Nested UDT fields: resolve against the keyspace's own definitions.
    for k in keyspaces.values_mut() {
        let defs = k.udt_defs();
        for u in k.types.values_mut() {
            u.fields = u
                .fields
                .iter()
                .map(|(n, t)| (n.clone(), t.resolved(&defs)))
                .collect();
        }
    }
    let udts_by_ks: BTreeMap<String, BTreeMap<String, Vec<(String, CqlType)>>> = keyspaces
        .iter()
        .map(|(n, k)| (n.clone(), k.udt_defs()))
        .collect();

    let col_rows = query_rows(session, "SELECT * FROM system_schema.columns").await?;
    // Resolve per keyspace (rows of one keyspace use that keyspace's types).
    let mut columns: BTreeMap<(String, String), Vec<ColumnInfo>> = BTreeMap::new();
    let mut grouped: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for r in col_rows {
        grouped
            .entry(r.string("keyspace_name"))
            .or_default()
            .push(r);
    }
    for (ks, rows) in grouped {
        let empty = BTreeMap::new();
        let defs = udts_by_ks.get(&ks).unwrap_or(&empty);
        columns.extend(build_columns(&rows, defs));
    }

    for r in query_rows(session, "SELECT * FROM system_schema.tables").await? {
        let ks = r.string("keyspace_name");
        let name = r.string("table_name");
        let cols = columns
            .remove(&(ks.clone(), name.clone()))
            .unwrap_or_default();
        if let Some(k) = keyspaces.get_mut(&ks) {
            k.tables.insert(name, table_from_row(&r, cols, false));
        }
    }
    for r in query_rows(session, "SELECT * FROM system_schema.views").await? {
        let ks = r.string("keyspace_name");
        let name = r.string("view_name");
        let cols = columns
            .remove(&(ks.clone(), name.clone()))
            .unwrap_or_default();
        if let Some(k) = keyspaces.get_mut(&ks) {
            k.views.insert(name, table_from_row(&r, cols, true));
        }
    }
    for r in query_rows(session, "SELECT * FROM system_schema.indexes").await? {
        let ks = r.string("keyspace_name");
        let options = r.text_map("options");
        let info = IndexInfo {
            name: r.string("index_name"),
            kind: r.string("kind"),
            target: options.get("target").cloned().unwrap_or_default(),
            class: options.get("class_name").cloned(),
            options,
        };
        if let Some(t) = keyspaces
            .get_mut(&ks)
            .and_then(|k| k.relation_mut(&r.string("table_name")))
        {
            t.indexes.push(info);
        }
    }
    if let Some(rows) = try_rows(session, "SELECT * FROM system_schema.triggers").await {
        for r in rows {
            let ks = r.string("keyspace_name");
            let opts = r.text_map("options");
            if let Some(t) = keyspaces
                .get_mut(&ks)
                .and_then(|k| k.tables.get_mut(&r.string("table_name")))
            {
                t.triggers.push((
                    r.string("trigger_name"),
                    opts.get("class").cloned().unwrap_or_default(),
                ));
            }
        }
    }
    for r in query_rows(session, "SELECT * FROM system_schema.functions").await? {
        let ks = r.string("keyspace_name");
        if let Some(k) = keyspaces.get_mut(&ks) {
            k.functions.push(FunctionInfo {
                name: r.string("function_name"),
                arg_names: r.text_list("argument_names"),
                arg_types: r.text_list("argument_types"),
                return_type: r.string("return_type"),
                language: r.string("language"),
                body: r.string("body"),
                called_on_null: r.bool("called_on_null_input").unwrap_or(false),
            });
        }
    }
    for r in query_rows(session, "SELECT * FROM system_schema.aggregates").await? {
        let ks = r.string("keyspace_name");
        if let Some(k) = keyspaces.get_mut(&ks) {
            k.aggregates.push(AggregateInfo {
                name: r.string("aggregate_name"),
                arg_types: r.text_list("argument_types"),
                state_func: r.string("state_func"),
                state_type: r.string("state_type"),
                final_func: r.str("final_func").filter(|s| !s.is_empty()),
                initcond: initcond_text(&r),
                return_type: r.string("return_type"),
            });
        }
    }
    // Virtual tables (Cassandra 4+): read-only, no options.
    if let Some(vks) = try_rows(session, "SELECT * FROM system_virtual_schema.keyspaces").await {
        let vcols = try_rows(session, "SELECT * FROM system_virtual_schema.columns")
            .await
            .unwrap_or_default();
        let empty = BTreeMap::new();
        let mut vcolumns = build_columns(&vcols, &empty);
        for r in vks {
            let name = r.string("keyspace_name");
            keyspaces
                .entry(name.clone())
                .or_insert_with(|| KeyspaceInfo {
                    name,
                    durable_writes: true,
                    is_virtual: true,
                    ..KeyspaceInfo::default()
                });
        }
        if let Some(rows) = try_rows(session, "SELECT * FROM system_virtual_schema.tables").await {
            for r in rows {
                let ks = r.string("keyspace_name");
                let name = r.string("table_name");
                let cols = vcolumns
                    .remove(&(ks.clone(), name.clone()))
                    .unwrap_or_default();
                if let Some(k) = keyspaces.get_mut(&ks) {
                    let mut t = table_from_row(&r, cols, false);
                    t.is_virtual = true;
                    k.tables.insert(name, t);
                }
            }
        }
    }
    Ok(Snapshot { keyspaces })
}

impl KeyspaceInfo {
    fn relation_mut(&mut self, name: &str) -> Option<&mut TableInfo> {
        if self.tables.contains_key(name) {
            self.tables.get_mut(name)
        } else {
            self.views.get_mut(name)
        }
    }
}

/// `initcond` is stored as text (a CQL literal).
fn initcond_text(r: &Row) -> Option<String> {
    r.str("initcond").filter(|s| !s.is_empty())
}

/// Snapshot cache of one session.
#[derive(Default)]
pub(crate) struct SchemaCache {
    slot: Mutex<Option<(Instant, Arc<Snapshot>)>>,
    gate: tokio::sync::Mutex<()>,
}

impl SchemaCache {
    pub(crate) fn invalidate(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn fresh(&self) -> Option<Arc<Snapshot>> {
        let guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match &*guard {
            Some((at, s)) if at.elapsed() < CACHE_TTL => Some(s.clone()),
            _ => None,
        }
    }

    /// Cached snapshot, loaded when missing or older than a minute.
    pub(crate) async fn get(&self, session: &Session) -> Result<Arc<Snapshot>> {
        if let Some(s) = self.fresh() {
            return Ok(s);
        }
        let _gate = self.gate.lock().await;
        if let Some(s) = self.fresh() {
            return Ok(s);
        }
        let snapshot = Arc::new(load(session).await?);
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), snapshot.clone()));
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_literals() {
        assert_eq!(
            option_literal(&CqlValue::Text("a'b".into())).unwrap(),
            "'a''b'"
        );
        assert_eq!(option_literal(&CqlValue::Int(3)).unwrap(), "3");
        assert_eq!(option_literal(&CqlValue::Double(0.01)).unwrap(), "0.01");
        let m = CqlValue::Map(vec![(
            CqlValue::Text("class".into()),
            CqlValue::Text("X".into()),
        )]);
        assert_eq!(option_literal(&m).unwrap(), "{'class': 'X'}");
    }

    #[test]
    fn system_keyspaces() {
        assert!(is_system_keyspace("system"));
        assert!(is_system_keyspace("system_schema"));
        assert!(is_system_keyspace("system_views"));
        assert!(!is_system_keyspace("shop"));
    }

    #[test]
    fn column_order_is_keys_first() {
        let row = |name: &str, kind: &str, pos: i32| {
            let mut r = Row::default();
            r.cols
                .push(("keyspace_name".into(), Some(CqlValue::Text("ks".into()))));
            r.cols
                .push(("table_name".into(), Some(CqlValue::Text("t".into()))));
            r.cols
                .push(("column_name".into(), Some(CqlValue::Text(name.into()))));
            r.cols
                .push(("kind".into(), Some(CqlValue::Text(kind.into()))));
            r.cols.push(("position".into(), Some(CqlValue::Int(pos))));
            r.cols.push((
                "clustering_order".into(),
                Some(CqlValue::Text("desc".into())),
            ));
            r.cols
                .push(("type".into(), Some(CqlValue::Text("int".into()))));
            r
        };
        let rows = vec![
            row("zz", "regular", -1),
            row("b", "clustering", 0),
            row("aa", "regular", -1),
            row("p2", "partition_key", 1),
            row("p1", "partition_key", 0),
        ];
        let cols = build_columns(&rows, &BTreeMap::new());
        let names: Vec<_> = cols[&("ks".into(), "t".into())]
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["p1", "p2", "b", "aa", "zz"]);
    }
}
