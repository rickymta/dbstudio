//! Server-side object search (`search_objects`) over the catalogs.
//!
//! Scope handling: the root searches every database the login can connect to, the session's
//! default database first (one connection per database, see [`root_databases`]); a `Database`
//! scope searches that database (through its own pool); a `Schema` scope searches that schema
//! only, system schemas included when named explicitly. Without a schema scope the system
//! schemas (`pg_catalog`, `information_schema`, `pg_toast*`) are skipped.

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

/// Databases one root search looks into at most: each one costs a connection.
pub const MAX_SEARCH_DATABASES: usize = 16;

/// Databases a search from the root covers, the login database first.
///
/// The tree lists every database of the server, so a search limited to the login database
/// (often `postgres`) found none of the tables and functions shown under the others.
pub fn root_databases(login: &str, connectable: Vec<String>) -> Vec<String> {
    let mut out = vec![login.to_string()];
    out.extend(connectable.into_iter().filter(|d| d != login));
    out.truncate(MAX_SEARCH_DATABASES);
    out
}

/// Rank of a result for `query`: 0 exact, 1 prefix, 2 substring (case-insensitive). A routine
/// is matched on its name, without the argument list.
fn rank(name: &str, query: &str) -> u8 {
    let base = name.split('(').next().unwrap_or(name).to_lowercase();
    let q = query.trim().to_lowercase();
    if base == q {
        0
    } else if base.starts_with(&q) {
        1
    } else {
        2
    }
}

/// One list out of the answers of several databases: best matches first, then by name; equal
/// names keep the order of the databases. At most `limit` (clamped like a single search).
pub fn merge(per_database: Vec<Vec<ObjectNode>>, query: &str, limit: u32) -> Vec<ObjectNode> {
    let mut all: Vec<(u8, String, ObjectNode)> = per_database
        .into_iter()
        .flatten()
        .map(|n| {
            let base = n.name.split('(').next().unwrap_or(&n.name).to_lowercase();
            (rank(&n.name, query), base, n)
        })
        .collect();
    all.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    all.truncate(limit.clamp(1, 1000) as usize);
    all.into_iter().map(|(_, _, n)| n).collect()
}

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
                            WHEN COALESCE(c.relispartition, false) THEN 'partition' END AS info \
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
                        THEN NOT (o.sch IN ('pg_catalog', 'information_schema', 'crdb_internal', 'pg_extension') \
                                  OR (left(o.sch::text, 8) = 'pg_toast') OR (left(o.sch::text, 8) = 'pg_temp_')) \
                        ELSE o.sch = $1::text END \
               AND position(lower($2::text) in lower(o.mname)) > 0 \
             ORDER BY CASE WHEN lower(o.mname) = lower($2::text) THEN 0 \
                           WHEN position(lower($2::text) in lower(o.mname)) = 1 THEN 1 \
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
    fn root_covers_every_database_login_first() {
        let dbs = |login: &str, all: &[&str]| {
            root_databases(login, all.iter().map(|d| d.to_string()).collect())
        };
        assert_eq!(
            dbs("postgres", &["app", "hub", "postgres"]),
            vec!["postgres", "app", "hub"]
        );
        // The login database is searched even when the list does not name it.
        assert_eq!(dbs("shop", &["app"]), vec!["shop", "app"]);
        let many: Vec<String> = (0..40).map(|n| format!("db{n:02}")).collect();
        assert_eq!(root_databases("db07", many).len(), MAX_SEARCH_DATABASES);
    }

    #[test]
    fn merges_databases_by_rank_then_name() {
        let n = |db: &str, kind: ObjectKind, name: &str| ObjectNode {
            path: object_path(db, "public", kind, name),
            kind,
            name: name.to_string(),
            has_children: false,
            detail: Some(format!("{db}.public")),
        };
        let merged = merge(
            vec![
                vec![n("postgres", ObjectKind::Table, "user_log")],
                vec![
                    n("app", ObjectKind::Table, "user"),
                    n("app", ObjectKind::Function, "find_user(id integer)"),
                    n("app", ObjectKind::Function, "User(text)"),
                ],
                vec![n("hub", ObjectKind::Table, "user")],
            ],
            " USER ",
            4,
        );
        let got: Vec<(String, String)> = merged
            .iter()
            .map(|x| (x.detail.clone().unwrap(), x.name.clone()))
            .collect();
        let want = [
            ("app.public", "user"),
            ("app.public", "User(text)"),
            ("hub.public", "user"),
            ("postgres.public", "user_log"),
        ];
        assert_eq!(
            got,
            want.map(|(d, name)| (d.to_string(), name.to_string()))
                .to_vec()
        );
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
