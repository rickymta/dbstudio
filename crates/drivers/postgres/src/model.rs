//! PostgreSQL driver: schema model (ERD, autocomplete). Owned by work package F.
//!
//! The whole model of a database or schema is read with four `pg_catalog` queries (relations,
//! columns, primary/foreign keys, unique indexes), all inside one read-only snapshot.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use dbstudio_core::{
    ColumnDetail, Error, ModelDetail, ObjectKind, ObjectPath, Relation, Result, SchemaModel,
    SchemaModelRequest, TableModel, ValueKind,
};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Row};

use crate::browse::object_path;
use crate::error::map_error;
use crate::session::PostgresSession;
use crate::sql::non_folder_segments;

/// More tables than this are cut off (`SchemaModel::truncated`).
pub const MAX_TABLES: usize = 2000;

tokio::task_local! {
    static CATALOG_COUNTER: Arc<AtomicU32>;
}

/// Run `f` and count the catalog queries issued by [`schema_model`](dbstudio_core::Session)
/// and `generate_ddl` on this task meanwhile. Used by the integration tests.
pub async fn count_catalog_queries<F: Future>(f: F) -> (F::Output, u32) {
    let counter = Arc::new(AtomicU32::new(0));
    let out = CATALOG_COUNTER.scope(counter.clone(), f).await;
    (out, counter.load(Ordering::SeqCst))
}

/// Run one catalog query, counting it when a counter is installed.
pub(crate) async fn catalog_query(
    client: &Client,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
) -> Result<Vec<Row>> {
    let _ = CATALOG_COUNTER.try_with(|c| c.fetch_add(1, Ordering::SeqCst));
    client.query(sql, params).await.map_err(map_error)
}

/// Where a request points: a database (all user schemas) or one schema of it.
pub(crate) struct Scope {
    pub database: Option<String>,
    pub schema: Option<String>,
}

pub(crate) fn parse_scope(path: &ObjectPath) -> Result<Scope> {
    use ObjectKind as K;
    match non_folder_segments(path).as_slice() {
        [] => Ok(Scope {
            database: None,
            schema: None,
        }),
        [(K::Database, d)] => Ok(Scope {
            database: Some((*d).to_string()),
            schema: None,
        }),
        [(K::Database, d), (K::Schema, s)] => Ok(Scope {
            database: Some((*d).to_string()),
            schema: Some((*s).to_string()),
        }),
        _ => Err(Error::NotFound(
            "scope must be a database or a schema".to_string(),
        )),
    }
}

/// `(schema, name)` of a relation path (`Database/Schema/Table|View|MaterializedView|...`).
pub(crate) fn relation_key(path: &ObjectPath) -> Option<(String, String)> {
    match non_folder_segments(path).as_slice() {
        [(ObjectKind::Database, _), (ObjectKind::Schema, s), (_, n)] => {
            Some(((*s).to_string(), (*n).to_string()))
        }
        _ => None,
    }
}

/// Common table expression selecting the relations of the scope; parameters `$1..$4`:
/// relkinds `text[]`, schema `text` (nullable), explicit schemas `text[]` and names `text[]`.
pub(crate) const RELS_CTE: &str = "\
WITH rels AS ( \
  SELECT c.oid, n.nspname, c.relname, c.relkind, c.reltuples, c.relispartition \
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
  WHERE c.relkind::text = ANY($1::text[]) \
    AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'crdb_internal', 'pg_extension') \
    AND n.nspname !~ '^pg_(toast|temp)' \
    AND ($2::text IS NULL OR n.nspname = $2::text) \
    AND (cardinality($3::text[]) = 0 \
         OR (n.nspname::text, c.relname::text) IN (SELECT * FROM unnest($3::text[], $4::text[]))) \
    AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass \
                    AND d.objid = c.oid AND d.deptype = 'e') \
  ORDER BY n.nspname, c.relname LIMIT ";

pub(crate) fn kind_of_relkind(relkind: &str) -> ObjectKind {
    match relkind {
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        _ => ObjectKind::Table,
    }
}

fn fk_action(code: &str) -> Option<String> {
    Some(
        match code {
            "a" => "NO ACTION",
            "r" => "RESTRICT",
            "c" => "CASCADE",
            "n" => "SET NULL",
            "d" => "SET DEFAULT",
            _ => return None,
        }
        .to_string(),
    )
}

struct TableAcc {
    model: TableModel,
    index: usize,
}

/// Schema model of a database or schema in four catalog queries.
pub(crate) async fn schema_model(
    session: &PostgresSession,
    req: SchemaModelRequest,
) -> Result<SchemaModel> {
    let scope = parse_scope(&req.scope)?;
    let database = scope
        .database
        .clone()
        .unwrap_or_else(|| session.login_db().to_string());
    let mut conn = session.shared.acquire_catalog(&database).await?;
    let client = &conn.client;
    client
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(map_error)?;
    let result = build(client, &database, &scope, &req, session.shared.flavor()).await;
    if client.batch_execute("ROLLBACK").await.is_ok() {
        conn.mark_reusable();
    }
    result.map(|mut m| {
        m.scope = req.scope.clone();
        m
    })
}

async fn build(
    client: &Client,
    database: &str,
    scope: &Scope,
    req: &SchemaModelRequest,
    flavor: crate::flavor::Flavor,
) -> Result<SchemaModel> {
    let hidden = crate::details::visible_filter(flavor, "a");
    let mut kinds: Vec<String> = ["r", "p", "f"].map(String::from).to_vec();
    if req.include_views {
        kinds.push("v".into());
        kinds.push("m".into());
    }
    let mut sel_schemas: Vec<String> = Vec::new();
    let mut sel_names: Vec<String> = Vec::new();
    for t in &req.tables {
        if let Some((s, n)) = relation_key(t) {
            sel_schemas.push(s);
            sel_names.push(n);
        }
    }
    if !req.tables.is_empty() && sel_schemas.is_empty() {
        return Ok(SchemaModel {
            scope: req.scope.clone(),
            tables: Vec::new(),
            relations: Vec::new(),
            inferred: false,
            truncated: false,
        });
    }
    let schema = scope.schema.clone();
    let comments = req.include_comments;

    // 1. relations
    let sql1 = format!(
        "{RELS_CTE}{} ) \
         SELECT r.nspname::text, r.relname::text, r.relkind::text, COALESCE(r.reltuples::int8, -1), \
                CASE WHEN $5::bool THEN obj_description(r.oid, 'pg_class') END \
         FROM rels r ORDER BY r.nspname, r.relname",
        MAX_TABLES + 1
    );
    let rows = catalog_query(
        client,
        &sql1,
        &[&kinds, &schema, &sel_schemas, &sel_names, &comments],
    )
    .await?;
    let truncated = rows.len() > MAX_TABLES;
    let mut tables: Vec<TableAcc> = Vec::new();
    let mut by_key: HashMap<(String, String), usize> = HashMap::new();
    for r in rows.iter().take(MAX_TABLES) {
        let nsp: String = r.get(0);
        let name: String = r.get(1);
        let kind = kind_of_relkind(&r.get::<_, String>(2));
        let reltuples: i64 = r.get(3);
        let idx = tables.len();
        by_key.insert((nsp.clone(), name.clone()), idx);
        tables.push(TableAcc {
            index: idx,
            model: TableModel {
                path: object_path(database, &nsp, kind, &name),
                kind,
                schema: Some(nsp),
                name,
                comment: r.get(4),
                columns: Vec::new(),
                primary_key: Vec::new(),
                unique_keys: Vec::new(),
                estimated_rows: (reltuples >= 0).then_some(reltuples as u64),
            },
        });
    }
    if tables.is_empty() {
        return Ok(SchemaModel {
            scope: req.scope.clone(),
            tables: Vec::new(),
            relations: Vec::new(),
            inferred: false,
            truncated,
        });
    }

    // 2. columns
    let sql2 = format!(
        "{RELS_CTE}{MAX_TABLES} ) \
         SELECT r.nspname::text, r.relname::text, a.attname::text, a.attnum::int4, \
                format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                pg_get_expr(d.adbin, d.adrelid), \
                CASE WHEN $5::bool THEN col_description(a.attrelid, a.attnum) END, \
                a.attidentity::text, a.attgenerated::text, \
                CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END \
         FROM rels r \
         JOIN pg_attribute a ON a.attrelid = r.oid AND a.attnum > 0 AND NOT a.attisdropped{hidden} \
         JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         ORDER BY r.nspname, r.relname, a.attnum"
    );
    let rows = catalog_query(
        client,
        &sql2,
        &[&kinds, &schema, &sel_schemas, &sel_names, &comments],
    )
    .await?;
    for r in &rows {
        let key = (r.get::<_, String>(0), r.get::<_, String>(1));
        let Some(&i) = by_key.get(&key) else { continue };
        let identity: String = r.get(8);
        let generated: String = r.get(9);
        let extra = match (identity.as_str(), generated.as_str()) {
            ("a", _) => Some("identity always".to_string()),
            ("d", _) => Some("identity by default".to_string()),
            (_, "s") => Some("generated stored".to_string()),
            (_, "v") => Some("generated virtual".to_string()),
            _ => None,
        };
        let type_oid: u32 = r.get(10);
        tables[i].model.columns.push(ColumnDetail {
            name: r.get(2),
            ordinal: r.get::<_, i32>(3).max(0) as u32,
            type_name: r.get(4),
            value_kind: Type::from_oid(type_oid)
                .map(|t| crate::decode::value_kind(&t))
                .unwrap_or(ValueKind::Text),
            nullable: !r.get::<_, bool>(5),
            default: r.get(6),
            comment: r.get(7),
            primary_key: false,
            extra,
        });
    }

    // 3. primary and foreign keys
    let sql3 = format!(
        "{RELS_CTE}{MAX_TABLES} ) \
         SELECT r.nspname::text, r.relname::text, c.conname::text, c.contype::text, \
                ARRAY(SELECT a.attname::text \
                      FROM unnest(c.conkey) WITH ORDINALITY k(attnum, ord) \
                      JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
                      ORDER BY k.ord)::text[], \
                rn.nspname::text, rc.relname::text, rc.relkind::text, \
                ARRAY(SELECT a.attname::text \
                      FROM unnest(c.confkey) WITH ORDINALITY k(attnum, ord) \
                      JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.attnum \
                      ORDER BY k.ord)::text[], \
                c.confupdtype::text, c.confdeltype::text \
         FROM rels r \
         JOIN pg_constraint c ON c.conrelid = r.oid \
              AND (c.contype = 'p' OR (c.contype = 'f' AND c.conparentid = 0)) \
         LEFT JOIN pg_class rc ON rc.oid = c.confrelid \
         LEFT JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
         ORDER BY r.nspname, r.relname, c.contype DESC, c.conname"
    );
    let rows = catalog_query(client, &sql3, &[&kinds, &schema, &sel_schemas, &sel_names]).await?;
    struct RawFk {
        from: usize,
        name: String,
        cols: Vec<String>,
        to: (String, String, ObjectKind),
        to_cols: Vec<String>,
        upd: Option<String>,
        del: Option<String>,
    }
    let mut fks: Vec<RawFk> = Vec::new();
    for r in &rows {
        let key = (r.get::<_, String>(0), r.get::<_, String>(1));
        let Some(&i) = by_key.get(&key) else { continue };
        let cols: Vec<String> = r.get(4);
        if r.get::<_, String>(3) == "p" {
            // A hidden key (CockroachDB `rowid`) is not part of the model.
            tables[i].model.primary_key = cols;
        } else {
            fks.push(RawFk {
                from: i,
                name: r.get(2),
                cols,
                to: (r.get(5), r.get(6), kind_of_relkind(&r.get::<_, String>(7))),
                to_cols: r.get(8),
                upd: fk_action(&r.get::<_, String>(9)),
                del: fk_action(&r.get::<_, String>(10)),
            });
        }
    }

    // 4. unique indexes
    let sql4 = format!(
        "{RELS_CTE}{MAX_TABLES} ) \
         SELECT r.nspname::text, r.relname::text, \
                ARRAY(SELECT a.attname::text \
                      FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(attnum, ord) \
                      JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                      WHERE k.ord <= i.indnkeyatts ORDER BY k.ord)::text[] \
         FROM rels r \
         JOIN pg_index i ON i.indrelid = r.oid AND i.indisunique AND NOT i.indisprimary \
              AND i.indisvalid AND i.indpred IS NULL AND i.indexprs IS NULL \
         JOIN pg_class ic ON ic.oid = i.indexrelid \
         ORDER BY r.nspname, r.relname, ic.relname"
    );
    let rows = catalog_query(client, &sql4, &[&kinds, &schema, &sel_schemas, &sel_names]).await?;
    for r in &rows {
        let key = (r.get::<_, String>(0), r.get::<_, String>(1));
        let Some(&i) = by_key.get(&key) else { continue };
        let cols: Vec<String> = r.get(2);
        if !cols.is_empty() && !tables[i].model.unique_keys.contains(&cols) {
            tables[i].model.unique_keys.push(cols);
        }
    }

    // Derived data.
    let mut relations = Vec::new();
    for fk in fks {
        let Some(&to_idx) = by_key.get(&(fk.to.0.clone(), fk.to.1.clone())) else {
            // The referenced table is outside the model (other schema, filtered out).
            continue;
        };
        let from = &tables[fk.from].model;
        let nullable = |name: &String| {
            from.columns
                .iter()
                .find(|c| &c.name == name)
                .is_none_or(|c| c.nullable)
        };
        let cols: BTreeSet<&String> = fk.cols.iter().collect();
        let same = |other: &Vec<String>| {
            !other.is_empty() && other.iter().collect::<BTreeSet<_>>() == cols
        };
        relations.push(Relation {
            name: fk.name,
            from_table: from.path.clone(),
            from_columns: fk.cols.clone(),
            to_table: tables[to_idx].model.path.clone(),
            to_columns: fk.to_cols,
            on_update: fk.upd,
            on_delete: fk.del,
            from_optional: fk.cols.iter().any(nullable),
            one_to_one: same(&from.primary_key) || from.unique_keys.iter().any(same),
            inferred: false,
        });
    }

    let keys_only = req.detail == ModelDetail::Keys;
    let mut out = Vec::with_capacity(tables.len());
    let fk_cols: Vec<BTreeSet<String>> = {
        let mut v = vec![BTreeSet::new(); tables.len()];
        for r in &relations {
            if let Some(&i) = by_key.get(&match relation_key(&r.from_table) {
                Some(k) => k,
                None => continue,
            }) {
                v[i].extend(r.from_columns.iter().cloned());
            }
        }
        v
    };
    for acc in tables {
        let mut m = acc.model;
        let uniq: BTreeSet<&String> = m.unique_keys.iter().flatten().collect();
        let visible: BTreeSet<String> = m.columns.iter().map(|c| c.name.clone()).collect();
        m.primary_key.retain(|c| visible.contains(c));
        let pk: BTreeSet<&String> = m.primary_key.iter().collect();
        let pk_names: Vec<String> = m.primary_key.clone();
        for c in &mut m.columns {
            c.primary_key = pk_names.contains(&c.name);
        }
        if keys_only {
            let fkc = &fk_cols[acc.index];
            let keep: Vec<bool> = m
                .columns
                .iter()
                .map(|c| pk.contains(&c.name) || uniq.contains(&c.name) || fkc.contains(&c.name))
                .collect();
            let mut it = keep.into_iter();
            m.columns.retain(|_| it.next().unwrap_or(false));
        }
        out.push(m);
    }
    Ok(SchemaModel {
        scope: req.scope.clone(),
        tables: out,
        relations,
        inferred: false,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parsing() {
        let db = ObjectPath::root().child(ObjectKind::Database, "d");
        let s = parse_scope(&db).unwrap();
        assert_eq!(s.database.as_deref(), Some("d"));
        assert!(s.schema.is_none());
        let sch = db
            .child(ObjectKind::Folder, "schemas")
            .child(ObjectKind::Schema, "s");
        assert_eq!(parse_scope(&sch).unwrap().schema.as_deref(), Some("s"));
        assert!(parse_scope(&sch.child(ObjectKind::Table, "t")).is_err());
    }

    #[test]
    fn relation_keys() {
        let p = object_path("d", "s", ObjectKind::Table, "t");
        assert_eq!(relation_key(&p), Some(("s".into(), "t".into())));
        assert_eq!(relation_key(&ObjectPath::root()), None);
    }

    #[test]
    fn relkinds_and_actions() {
        assert_eq!(kind_of_relkind("m"), ObjectKind::MaterializedView);
        assert_eq!(kind_of_relkind("p"), ObjectKind::Table);
        assert_eq!(fk_action("n").as_deref(), Some("SET NULL"));
    }
}
