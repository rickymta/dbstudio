//! Server-side object search (`search_objects`) over the catalogs.
//!
//! Scope handling: the root searches only the session's default database (searching every
//! database would need a connection per database); a `Database` scope searches that database
//! (through its own pool); a `Schema` scope searches that schema only, system schemas included
//! when named explicitly. Without a schema scope the system schemas (`pg_catalog`,
//! `information_schema`, `pg_toast*`) are skipped.

use dbstudio_core::{Error, ObjectKind, ObjectNode, ObjectSearchRequest, Result};
use tokio_postgres::Client;

use crate::browse::object_path;
use crate::error::map_error;
use crate::sql::non_folder_segments;

const DEFAULT_KINDS: [ObjectKind; 6] = [
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Sequence,
];

/// Where a search runs.
#[derive(Debug, PartialEq, Eq)]
pub struct Scope {
    pub database: Option<String>,
    pub schema: Option<String>,
}

pub fn parse_scope(req: &ObjectSearchRequest) -> Result<Scope> {
    use ObjectKind as K;
    match non_folder_segments(&req.scope).as_slice() {
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
            "search scope must be the root, a database or a schema".to_string(),
        )),
    }
}

fn kind_key(kind: ObjectKind) -> Option<&'static str> {
    Some(match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materializedView",
        ObjectKind::Function => "function",
        ObjectKind::Procedure => "procedure",
        ObjectKind::Sequence => "sequence",
        _ => return None,
    })
}

fn kind_from_key(key: &str) -> ObjectKind {
    match key {
        "view" => ObjectKind::View,
        "materializedView" => ObjectKind::MaterializedView,
        "function" => ObjectKind::Function,
        "procedure" => ObjectKind::Procedure,
        "sequence" => ObjectKind::Sequence,
        _ => ObjectKind::Table,
    }
}

pub async fn search(
    client: &Client,
    req: &ObjectSearchRequest,
    scope: &Scope,
    database: &str,
    timeout: std::time::Duration,
) -> Result<Vec<ObjectNode>> {
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let kinds: Vec<String> = if req.kinds.is_empty() {
        DEFAULT_KINDS
            .iter()
            .filter_map(|k| kind_key(*k))
            .map(String::from)
            .collect()
    } else {
        req.kinds
            .iter()
            .filter_map(|k| kind_key(*k))
            .map(String::from)
            .collect()
    };
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let limit = i64::from(req.limit.clamp(1, 1000));
    // A short transaction carries `SET LOCAL statement_timeout`, so a search on a huge catalog
    // cannot hold the connection indefinitely.
    client
        .batch_execute(&format!(
            "BEGIN; SET LOCAL statement_timeout = {}",
            timeout.as_millis().max(1)
        ))
        .await
        .map_err(map_error)?;
    let result = client
        .query(
            "SELECT o.sch, o.name, o.kind, o.info FROM ( \
                SELECT n.nspname::text AS sch, c.relname::text AS name, c.relname::text AS mname, \
                       CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materializedView' \
                                      WHEN 'S' THEN 'sequence' ELSE 'table' END AS kind, \
                       CASE WHEN c.relkind = 'p' THEN 'partitioned' \
                            WHEN c.relkind = 'f' THEN 'foreign' \
                            WHEN c.relispartition THEN 'partition' END AS info \
                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm', 'S') \
                  AND NOT (c.relkind = 'S' AND EXISTS ( \
                        SELECT 1 FROM pg_depend d \
                        WHERE d.objid = c.oid AND d.classid = 'pg_class'::regclass \
                          AND d.deptype = 'i')) \
                UNION ALL \
                SELECT n.nspname::text, \
                       p.proname::text || '(' || pg_get_function_identity_arguments(p.oid) || ')', \
                       p.proname::text, \
                       CASE WHEN p.prokind = 'p' THEN 'procedure' ELSE 'function' END, NULL \
                FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                WHERE p.prokind IN ('f', 'w', 'p') \
             ) o \
             WHERE o.kind = ANY ($3::text[]) \
               AND CASE WHEN $1::text IS NULL \
                        THEN NOT (o.sch IN ('pg_catalog', 'information_schema') \
                                  OR starts_with(o.sch, 'pg_toast') OR starts_with(o.sch, 'pg_temp_')) \
                        ELSE o.sch = $1::text END \
               AND position(lower($2::text) in lower(o.mname)) > 0 \
             ORDER BY CASE WHEN lower(o.mname) = lower($2::text) THEN 0 \
                           WHEN starts_with(lower(o.mname), lower($2::text)) THEN 1 \
                           ELSE 2 END, \
                      o.mname, o.sch, o.name \
             LIMIT $4",
            &[&scope.schema, &query, &kinds, &limit],
        )
        .await;
    let rolled_back = client.batch_execute("ROLLBACK").await.is_ok();
    let rows = match result {
        Ok(rows) => rows,
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) => {
            return Err(Error::Timeout);
        }
        Err(e) => return Err(map_error(e)),
    };
    if !rolled_back {
        return Err(Error::Connection(
            "could not end the search transaction".into(),
        ));
    }
    let with_db = scope.database.is_none() && scope.schema.is_none();
    Ok(rows
        .iter()
        .map(|r| {
            let schema: String = r.get(0);
            let name: String = r.get(1);
            let kind = kind_from_key(&r.get::<_, String>(2));
            let info: Option<String> = r.get(3);
            let context = if with_db {
                format!("{database}.{schema}")
            } else {
                schema.clone()
            };
            let detail = match info {
                Some(i) => format!("{context} ({i})"),
                None => context,
            };
            ObjectNode {
                path: object_path(database, &schema, kind, &name),
                kind,
                name,
                has_children: matches!(
                    kind,
                    ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
                ),
                detail: Some(detail),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ObjectPath;

    fn req(scope: ObjectPath) -> ObjectSearchRequest {
        ObjectSearchRequest {
            query: "x".into(),
            scope,
            kinds: vec![],
            limit: 10,
        }
    }

    #[test]
    fn scopes() {
        use ObjectKind as K;
        assert_eq!(
            parse_scope(&req(ObjectPath::root())).unwrap(),
            Scope {
                database: None,
                schema: None
            }
        );
        let db = ObjectPath::root().child(K::Database, "d");
        assert_eq!(
            parse_scope(&req(db.clone())).unwrap().database.as_deref(),
            Some("d")
        );
        let schema = db.child(K::Folder, "schemas").child(K::Schema, "s");
        let s = parse_scope(&req(schema)).unwrap();
        assert_eq!(
            (s.database.as_deref(), s.schema.as_deref()),
            (Some("d"), Some("s"))
        );
        let bad = db.child(K::Folder, "roles");
        assert!(parse_scope(&req(bad.child(K::Role, "r"))).is_err());
    }
}
