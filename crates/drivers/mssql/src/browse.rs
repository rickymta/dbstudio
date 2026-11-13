//! The object tree, `table_details` and per-object DDL.
//!
//! Tree (paths include the `Folder` segments, listing also accepts paths without them):
//! root -> `Database`s and `Folder roles`; `Database` -> `Folder schemas` -> `Schema` ->
//! `Folder tables|views|functions|procedures|sequences|synonyms|types` -> objects; a table or
//! view -> `Folder columns|indexes|triggers`.

use dbstudio_core::{
    ColumnDetail, Error, ForeignKeyDetail, IndexDetail, ObjectKind, ObjectNode, ObjectPath,
    PathSegment, Result, TableDetails, folder,
};

use crate::catalog::{self, Col};
use crate::conn::Conn;
use crate::ddl::{self, Names};
use crate::session::MssqlSession;
use crate::sql::{ObjRef, lit, object_path, parse_object, quote, schema_path};
use crate::value::{bool_at, int_at, kind_of_type_name, opt_str_at, str_at};

pub(crate) fn node(
    path: ObjectPath,
    kind: ObjectKind,
    name: &str,
    has_children: bool,
    detail: Option<String>,
) -> ObjectNode {
    ObjectNode {
        path,
        kind,
        name: name.to_string(),
        has_children,
        detail,
    }
}

fn folder_node(parent: &ObjectPath, key: &str, count: i64) -> ObjectNode {
    node(
        parent.child(ObjectKind::Folder, key),
        ObjectKind::Folder,
        key,
        count > 0,
        Some(count.to_string()),
    )
}

/// Where a path points.
struct Ctx {
    db: Option<String>,
    schema: Option<String>,
    object: Option<(ObjectKind, String)>,
    last: Option<PathSegment>,
}

fn context(path: &ObjectPath) -> Ctx {
    let mut c = Ctx {
        db: None,
        schema: None,
        object: None,
        last: path.segments.last().cloned(),
    };
    for seg in &path.segments {
        match seg.kind {
            ObjectKind::Database => c.db = Some(seg.name.clone()),
            ObjectKind::Schema => c.schema = Some(seg.name.clone()),
            k if crate::sql::is_schema_object(k) && c.object.is_none() => {
                c.object = Some((k, seg.name.clone()));
            }
            _ => {}
        }
    }
    c
}

/// Children of `parent`.
pub async fn list_children(sess: &MssqlSession, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let cx = context(parent);
    let Some(last) = cx.last.clone() else {
        return root(sess).await;
    };
    match last.kind {
        ObjectKind::Folder => match last.name.as_str() {
            folder::ROLES if cx.db.is_none() => roles(sess, parent).await,
            folder::SCHEMAS => schemas(sess, parent, db_of(sess, &cx)).await,
            folder::COLUMNS | folder::INDEXES | folder::TRIGGERS => {
                let obj = object_of(&cx)?;
                object_folder(sess, parent, &obj, &last.name).await
            }
            key => {
                let schema = schema_of(&cx)?;
                schema_folder(sess, parent, &db_of(sess, &cx), &schema, key).await
            }
        },
        ObjectKind::Database => Ok(vec![node(
            parent.child(ObjectKind::Folder, folder::SCHEMAS),
            ObjectKind::Folder,
            folder::SCHEMAS,
            true,
            None,
        )]),
        ObjectKind::Schema => {
            let db = db_of(sess, &cx);
            schema_folders(sess, parent, &db, &last.name).await
        }
        ObjectKind::Table | ObjectKind::View => {
            let obj = object_of(&cx)?;
            object_folders(sess, parent, &obj).await
        }
        _ => Ok(Vec::new()),
    }
}

fn db_of(sess: &MssqlSession, cx: &Ctx) -> String {
    cx.db
        .clone()
        .unwrap_or_else(|| sess.login_db().to_string())
}

fn schema_of(cx: &Ctx) -> Result<String> {
    Ok(cx.schema.clone().unwrap_or_else(|| "dbo".to_string()))
}

fn object_of(cx: &Ctx) -> Result<ObjRef> {
    let (kind, name) = cx
        .object
        .clone()
        .ok_or_else(|| Error::NotFound("the path does not name a table or view".into()))?;
    Ok(ObjRef {
        db: cx.db.clone(),
        schema: cx.schema.clone().unwrap_or_else(|| "dbo".into()),
        name,
        kind,
        child: None,
    })
}

async fn root(sess: &MssqlSession) -> Result<Vec<ObjectNode>> {
    let mut conn = sess.shared.acquire_catalog(sess.login_db()).await?;
    let rows = conn
        .rows(
            "SELECT name, CASE WHEN database_id <= 4 THEN 1 ELSE 0 END FROM sys.databases \
             WHERE state = 0 AND HAS_DBACCESS(name) = 1 ORDER BY name",
        )
        .await?;
    let roles = conn
        .rows(
            "SELECT COUNT(*) FROM sys.server_principals \
             WHERE type IN ('S','U','G','R','E','X') AND name NOT LIKE N'##%'",
        )
        .await
        .map(|r| r.first().map_or(0, |x| int_at(x, 0)))
        .unwrap_or(0);
    conn.release();
    let root = ObjectPath::root();
    let mut out: Vec<ObjectNode> = rows
        .iter()
        .map(|r| {
            let name = str_at(r, 0);
            node(
                root.child(ObjectKind::Database, name.clone()),
                ObjectKind::Database,
                &name,
                true,
                (int_at(r, 1) == 1).then(|| "system".to_string()),
            )
        })
        .collect();
    if roles > 0 {
        out.push(folder_node(&root, folder::ROLES, roles));
    }
    Ok(out)
}

async fn roles(sess: &MssqlSession, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
    let mut conn = sess.shared.acquire_catalog(sess.login_db()).await?;
    let rows = conn
        .rows(
            "SELECT name, type_desc FROM sys.server_principals \
             WHERE type IN ('S','U','G','R','E','X') AND name NOT LIKE N'##%' ORDER BY name",
        )
        .await?;
    conn.release();
    Ok(rows
        .iter()
        .map(|r| {
            let name = str_at(r, 0);
            node(
                parent.child(ObjectKind::Role, name.clone()),
                ObjectKind::Role,
                &name,
                false,
                Some(str_at(r, 1).to_ascii_lowercase().replace('_', " ")),
            )
        })
        .collect())
}

async fn schemas(sess: &MssqlSession, parent: &ObjectPath, db: String) -> Result<Vec<ObjectNode>> {
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let rows = conn
        .rows(
            "SELECT s.name FROM sys.schemas s \
             WHERE s.schema_id NOT BETWEEN 16384 AND 16399 ORDER BY s.name",
        )
        .await?;
    conn.release();
    Ok(rows
        .iter()
        .map(|r| {
            let name = str_at(r, 0);
            let system = name == "sys" || name == "INFORMATION_SCHEMA";
            node(
                parent.child(ObjectKind::Schema, name.clone()),
                ObjectKind::Schema,
                &name,
                true,
                system.then(|| "system".to_string()),
            )
        })
        .collect())
}

/// Object-type letters of each schema folder.
fn type_codes(key: &str) -> &'static str {
    match key {
        folder::TABLES => "'U'",
        folder::VIEWS => "'V'",
        folder::FUNCTIONS => "'FN','IF','TF','FS','FT'",
        folder::PROCEDURES => "'P','PC','RF','X'",
        folder::SEQUENCES => "'SO'",
        folder::SYNONYMS => "'SN'",
        _ => "''",
    }
}

async fn schema_folders(
    sess: &MssqlSession,
    parent: &ObjectPath,
    db: &str,
    schema: &str,
) -> Result<Vec<ObjectNode>> {
    let mut conn = sess.shared.acquire_catalog(db).await?;
    let mut counts = Vec::new();
    for key in [
        folder::TABLES,
        folder::VIEWS,
        folder::FUNCTIONS,
        folder::PROCEDURES,
        folder::SEQUENCES,
        folder::SYNONYMS,
    ] {
        counts.push((key, 0i64));
    }
    let rows = conn
        .rows(&format!(
            "SELECT o.type, COUNT(*) FROM sys.objects o WHERE o.schema_id = SCHEMA_ID({}) \
             AND o.type IN ('U','V','FN','IF','TF','FS','FT','P','PC','RF','X','SO','SN') GROUP BY o.type",
            lit(schema)
        ))
        .await?;
    for r in &rows {
        let t = str_at(r, 0);
        let t = t.trim();
        let n = int_at(r, 1);
        for (key, count) in counts.iter_mut() {
            if type_codes(key).contains(&format!("'{t}'")) {
                *count += n;
            }
        }
    }
    let types = conn
        .rows(&format!(
            "SELECT COUNT(*) FROM sys.types WHERE schema_id = SCHEMA_ID({}) AND is_user_defined = 1",
            lit(schema)
        ))
        .await
        .map(|r| r.first().map_or(0, |x| int_at(x, 0)))
        .unwrap_or(0);
    conn.release();
    let mut out: Vec<ObjectNode> = counts
        .into_iter()
        .map(|(k, n)| folder_node(parent, k, n))
        .collect();
    out.push(folder_node(parent, folder::TYPES, types));
    Ok(out)
}

fn function_detail(t: &str) -> &'static str {
    match t.trim() {
        "FN" => "scalar",
        "IF" => "inline table",
        "TF" => "table",
        "FS" => "scalar (CLR)",
        "FT" => "table (CLR)",
        _ => "",
    }
}

async fn schema_folder(
    sess: &MssqlSession,
    parent: &ObjectPath,
    db: &str,
    schema: &str,
    key: &str,
) -> Result<Vec<ObjectNode>> {
    let mut conn = sess.shared.acquire_catalog(db).await?;
    let out = match key {
        folder::TYPES => {
            let rows = conn
                .rows(&format!(
                    "SELECT t.name, t.is_table_type, t.is_assembly_type, bt.name, t.max_length, t.precision, t.scale \
                     FROM sys.types t LEFT JOIN sys.types bt ON bt.user_type_id = t.system_type_id AND bt.is_user_defined = 0 \
                     WHERE t.schema_id = SCHEMA_ID({}) AND t.is_user_defined = 1 ORDER BY t.name",
                    lit(schema)
                ))
                .await?;
            rows.iter()
                .map(|r| {
                    let name = str_at(r, 0);
                    let detail = if bool_at(r, 1) {
                        "table type".to_string()
                    } else if bool_at(r, 2) {
                        "CLR".to_string()
                    } else {
                        crate::sql::format_type(&str_at(r, 3), int_at(r, 4), int_at(r, 5), int_at(r, 6))
                    };
                    node(
                        parent.child(ObjectKind::Type, name.clone()),
                        ObjectKind::Type,
                        &name,
                        false,
                        Some(detail),
                    )
                })
                .collect()
        }
        folder::TABLES | folder::VIEWS | folder::FUNCTIONS | folder::PROCEDURES
        | folder::SEQUENCES | folder::SYNONYMS => {
            let extra = match key {
                folder::SEQUENCES => "(SELECT TYPE_NAME(sq.user_type_id) FROM sys.sequences sq WHERE sq.object_id = o.object_id)",
                folder::SYNONYMS => "(SELECT sn.base_object_name FROM sys.synonyms sn WHERE sn.object_id = o.object_id)",
                folder::TABLES => "(SELECT CASE WHEN tb.is_memory_optimized = 1 THEN 'memory-optimized' WHEN tb.temporal_type = 2 THEN 'temporal' WHEN tb.is_external = 1 THEN 'external' END FROM sys.tables tb WHERE tb.object_id = o.object_id)",
                _ => "NULL",
            };
            let rows = conn
                .rows(&format!(
                    "SELECT o.name, o.type, {extra} FROM sys.objects o \
                     WHERE o.schema_id = SCHEMA_ID({}) AND o.type IN ({}) ORDER BY o.name",
                    lit(schema),
                    type_codes(key)
                ))
                .await?;
            let kind = match key {
                folder::TABLES => ObjectKind::Table,
                folder::VIEWS => ObjectKind::View,
                folder::FUNCTIONS => ObjectKind::Function,
                folder::PROCEDURES => ObjectKind::Procedure,
                folder::SEQUENCES => ObjectKind::Sequence,
                _ => ObjectKind::Synonym,
            };
            rows.iter()
                .map(|r| {
                    let name = str_at(r, 0);
                    let detail = match kind {
                        ObjectKind::Function => Some(function_detail(&str_at(r, 1)).to_string()),
                        ObjectKind::Procedure => {
                            (str_at(r, 1).trim() == "PC").then(|| "CLR".to_string())
                        }
                        _ => opt_str_at(r, 2),
                    };
                    node(
                        parent.child(kind, name.clone()),
                        kind,
                        &name,
                        matches!(kind, ObjectKind::Table | ObjectKind::View),
                        detail,
                    )
                })
                .collect()
        }
        other => return Err(Error::NotFound(format!("unknown folder `{other}`"))),
    };
    conn.release();
    Ok(out)
}

async fn object_folders(
    sess: &MssqlSession,
    parent: &ObjectPath,
    obj: &ObjRef,
) -> Result<Vec<ObjectNode>> {
    let db = obj.db.clone().unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let oid = format!("OBJECT_ID({})", lit(&obj.qualified()));
    let rows = conn
        .rows(&format!(
            "SELECT (SELECT COUNT(*) FROM sys.columns WHERE object_id = {oid}), \
             (SELECT COUNT(DISTINCT index_id) FROM sys.indexes WHERE object_id = {oid} AND index_id > 0), \
             (SELECT COUNT(*) FROM sys.triggers WHERE parent_id = {oid}), {oid}"
        ))
        .await?;
    conn.release();
    let r = rows.first().cloned().unwrap_or_default();
    if opt_str_at(&r, 3).is_none() {
        return Err(Error::NotFound(format!("object {}", obj.qualified())));
    }
    Ok(vec![
        folder_node(parent, folder::COLUMNS, int_at(&r, 0)),
        folder_node(parent, folder::INDEXES, int_at(&r, 1)),
        folder_node(parent, folder::TRIGGERS, int_at(&r, 2)),
    ])
}

fn trigger_detail(rows: &[String], instead: bool, disabled: bool) -> String {
    let mut s = format!(
        "{} {}",
        if instead { "INSTEAD OF" } else { "AFTER" },
        rows.join(", ")
    );
    if disabled {
        s.push_str(", disabled");
    }
    s
}

async fn object_folder(
    sess: &MssqlSession,
    parent: &ObjectPath,
    obj: &ObjRef,
    key: &str,
) -> Result<Vec<ObjectNode>> {
    // `parent` ends with the folder; children hang below the table path (without the folder
    // segment), matching `[Table t, Folder columns, Column c]` exactly: parent already holds it.
    let db = obj.db.clone().unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let filter = catalog::one(&obj.schema, &obj.name);
    let out = match key {
        folder::COLUMNS => {
            let cols = catalog::columns(&mut conn, &filter).await?;
            let list = cols.into_values().next().unwrap_or_default();
            if list.is_empty() {
                ensure_exists(&mut conn, obj).await?;
            }
            list.iter()
                .map(|c| {
                    let mut detail = c.display_type.clone();
                    if c.identity.is_some() {
                        detail.push_str(" identity");
                    }
                    if c.computed.is_some() {
                        detail.push_str(" computed");
                    }
                    node(
                        parent.child(ObjectKind::Column, c.name.clone()),
                        ObjectKind::Column,
                        &c.name,
                        false,
                        Some(detail),
                    )
                })
                .collect()
        }
        folder::INDEXES => {
            let idx = catalog::indexes(&mut conn, &filter).await?;
            idx.into_values()
                .next()
                .unwrap_or_default()
                .iter()
                .map(|i| {
                    let cols: Vec<String> = i
                        .keys
                        .iter()
                        .map(|(c, d)| if *d { format!("{c} DESC") } else { c.clone() })
                        .collect();
                    let detail = format!(
                        "{}{}({})",
                        if i.primary {
                            "PRIMARY KEY "
                        } else if i.unique_constraint {
                            "UNIQUE "
                        } else if i.unique {
                            "UNIQUE INDEX "
                        } else {
                            ""
                        },
                        i.type_desc.to_ascii_lowercase(),
                        cols.join(", ")
                    );
                    node(
                        parent.child(ObjectKind::Index, i.name.clone()),
                        ObjectKind::Index,
                        &i.name,
                        false,
                        Some(detail),
                    )
                })
                .collect()
        }
        folder::TRIGGERS => {
            let rows = conn
                .rows(&format!(
                    "SELECT tr.name, tr.is_instead_of_trigger, tr.is_disabled, \
                     (SELECT STRING_AGG(te.type_desc, ', ') WITHIN GROUP (ORDER BY te.type_desc) \
                      FROM sys.trigger_events te WHERE te.object_id = tr.object_id) \
                     FROM sys.triggers tr WHERE tr.parent_id = OBJECT_ID({}) ORDER BY tr.name",
                    lit(&obj.qualified())
                ))
                .await?;
            rows.iter()
                .map(|r| {
                    let name = str_at(r, 0);
                    let events: Vec<String> = str_at(r, 3).split(", ").map(String::from).collect();
                    node(
                        parent.child(ObjectKind::Trigger, name.clone()),
                        ObjectKind::Trigger,
                        &name,
                        false,
                        Some(trigger_detail(&events, bool_at(r, 1), bool_at(r, 2))),
                    )
                })
                .collect()
        }
        other => return Err(Error::NotFound(format!("unknown folder `{other}`"))),
    };
    conn.release();
    Ok(out)
}

async fn ensure_exists(conn: &mut Conn, obj: &ObjRef) -> Result<()> {
    let rows = conn
        .rows(&format!("SELECT OBJECT_ID({})", lit(&obj.qualified())))
        .await?;
    if rows.first().and_then(|r| opt_str_at(r, 0)).is_none() {
        return Err(Error::NotFound(format!("object {}", obj.qualified())));
    }
    Ok(())
}

/// `table_details` of a table or view.
pub async fn table_details(sess: &MssqlSession, path: &ObjectPath) -> Result<TableDetails> {
    let obj = parse_object(path)?;
    let db = obj.db.clone().unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let filter = catalog::one(&obj.schema, &obj.name);
    let key = (obj.schema.clone(), obj.name.clone());
    let cols = catalog::columns(&mut conn, &filter)
        .await?
        .remove(&key)
        .unwrap_or_default();
    if cols.is_empty() {
        ensure_exists(&mut conn, &obj).await?;
    }
    let idx = catalog::indexes(&mut conn, &filter)
        .await?
        .remove(&key)
        .unwrap_or_default();
    let fks = catalog::foreign_keys(&mut conn, &filter)
        .await?
        .remove(&key)
        .unwrap_or_default();
    let comment = catalog::table_comments(&mut conn, &filter)
        .await?
        .remove(&key);
    let stat = conn
        .rows(&format!(
            "SELECT SUM(CASE WHEN p.index_id IN (0, 1) THEN p.rows END), \
             (SELECT SUM(a.total_pages) * 8192 FROM sys.partitions p2 \
              JOIN sys.allocation_units a ON (a.type IN (1, 3) AND a.container_id = p2.hobt_id) \
              OR (a.type = 2 AND a.container_id = p2.partition_id) \
              WHERE p2.object_id = OBJECT_ID({q})), \
             (SELECT type FROM sys.objects WHERE object_id = OBJECT_ID({q})) \
             FROM sys.partitions p WHERE p.object_id = OBJECT_ID({q})",
            q = lit(&obj.qualified())
        ))
        .await?;
    conn.release();
    let st = stat.first().cloned().unwrap_or_default();
    let kind = if str_at(&st, 2).trim() == "V" {
        ObjectKind::View
    } else {
        ObjectKind::Table
    };
    let pk: Vec<&str> = idx
        .iter()
        .find(|i| i.primary)
        .map(|i| i.keys.iter().map(|(c, _)| c.as_str()).collect())
        .unwrap_or_default();
    let columns = cols
        .iter()
        .enumerate()
        .map(|(n, c)| column_detail(n, c, pk.contains(&c.name.as_str())))
        .collect();
    let indexes = idx
        .iter()
        .map(|i| IndexDetail {
            name: i.name.clone(),
            columns: i
                .keys
                .iter()
                .map(|(c, d)| if *d { format!("{c} DESC") } else { c.clone() })
                .collect(),
            unique: i.unique,
            primary: i.primary,
            method: Some(i.type_desc.clone()),
            definition: ddl::create_index(Names { qualify: true }, &obj.schema, &obj.name, i),
        })
        .collect();
    let foreign_keys = fks
        .iter()
        .map(|f| ForeignKeyDetail {
            name: f.name.clone(),
            columns: f.cols.clone(),
            referenced_table: object_path(&db, &f.ref_schema, ObjectKind::Table, &f.ref_table),
            referenced_columns: f.ref_cols.clone(),
            on_update: Some(f.on_update.clone()),
            on_delete: Some(f.on_delete.clone()),
        })
        .collect();
    let rows_est = match st.first() {
        Some(dbstudio_core::Value::Null) | None => None,
        _ => Some(int_at(&st, 0).max(0) as u64),
    };
    let size = match st.get(1) {
        Some(dbstudio_core::Value::Null) | None => None,
        _ => Some(int_at(&st, 1).max(0) as u64),
    };
    Ok(TableDetails {
        path: object_path(&db, &obj.schema, kind, &obj.name),
        kind,
        comment,
        columns,
        indexes,
        foreign_keys,
        estimated_rows: rows_est,
        size_bytes: size,
    })
}

pub(crate) fn column_detail(n: usize, c: &Col, primary_key: bool) -> ColumnDetail {
    let mut extra: Vec<String> = Vec::new();
    if let Some((seed, inc)) = &c.identity {
        extra.push(format!("identity({seed},{inc})"));
    }
    if let Some((_, persisted)) = &c.computed {
        extra.push(if *persisted { "computed persisted" } else { "computed" }.to_string());
    }
    if c.rowversion {
        extra.push("rowversion".into());
    }
    if c.generated == 1 {
        extra.push("generated always row start".into());
    } else if c.generated == 2 {
        extra.push("generated always row end".into());
    }
    if c.sparse {
        extra.push("sparse".into());
    }
    ColumnDetail {
        name: c.name.clone(),
        ordinal: (n + 1) as u32,
        type_name: c.display_type.clone(),
        value_kind: kind_of_type_name(&c.base_type),
        nullable: c.nullable,
        default: c
            .default
            .as_ref()
            .map(|(_, d)| d.clone())
            .or_else(|| c.computed.as_ref().map(|(e, _)| e.clone())),
        comment: c.comment.clone(),
        primary_key,
        extra: (!extra.is_empty()).then(|| extra.join(", ")),
    }
}

/// `object_ddl`: the statements that recreate the object, batches separated by `GO`.
pub async fn object_ddl(sess: &MssqlSession, path: &ObjectPath) -> Result<String> {
    // Databases and schemas have their own, short scripts.
    if let Some(last) = path.segments.iter().rfind(|s| s.kind != ObjectKind::Folder) {
        match last.kind {
            ObjectKind::Database => return database_ddl(sess, &last.name).await,
            ObjectKind::Schema => {
                let db = path
                    .segments
                    .iter()
                    .find(|s| s.kind == ObjectKind::Database)
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| sess.login_db().to_string());
                return schema_ddl(sess, &db, &last.name).await;
            }
            ObjectKind::Role => {
                return Ok(format!("CREATE LOGIN {} FROM WINDOWS;", quote(&last.name)));
            }
            _ => {}
        }
    }
    let obj = parse_object(path)?;
    let db = obj.db.clone().unwrap_or_else(|| sess.login_db().to_string());
    let mut conn = sess.shared.acquire_catalog(&db).await?;
    let result = object_ddl_on(&mut conn, &obj, &db).await;
    conn.release();
    result
}

async fn database_ddl(sess: &MssqlSession, db: &str) -> Result<String> {
    let mut conn = sess.shared.acquire_catalog(sess.login_db()).await?;
    let rows = conn
        .rows(&format!(
            "SELECT collation_name, compatibility_level FROM sys.databases WHERE name = {}",
            lit(db)
        ))
        .await?;
    conn.release();
    let r = rows
        .first()
        .ok_or_else(|| Error::NotFound(format!("database {db}")))?;
    let mut s = format!("CREATE DATABASE {}", quote(db));
    if let Some(c) = opt_str_at(r, 0) {
        s.push_str(&format!("\nCOLLATE {c}"));
    }
    s.push(';');
    Ok(s)
}

async fn schema_ddl(sess: &MssqlSession, db: &str, schema: &str) -> Result<String> {
    let mut conn = sess.shared.acquire_catalog(db).await?;
    let rows = conn
        .rows(&format!(
            "SELECT USER_NAME(principal_id) FROM sys.schemas WHERE name = {}",
            lit(schema)
        ))
        .await?;
    conn.release();
    let r = rows
        .first()
        .ok_or_else(|| Error::NotFound(format!("schema {schema}")))?;
    let mut s = format!("CREATE SCHEMA {}", quote(schema));
    if let Some(owner) = opt_str_at(r, 0) {
        s.push_str(&format!(" AUTHORIZATION {}", quote(&owner)));
    }
    s.push(';');
    Ok(s)
}

/// Text of `OBJECT_DEFINITION` for a module (view, procedure, function, trigger).
pub(crate) async fn module_definition(conn: &mut Conn, obj: &ObjRef) -> Result<String> {
    let rows = conn
        .rows(&format!(
            "SELECT OBJECT_DEFINITION(OBJECT_ID({})), OBJECT_ID({})",
            lit(&obj.qualified()),
            lit(&obj.qualified())
        ))
        .await?;
    let r = rows.first().cloned().unwrap_or_default();
    if opt_str_at(&r, 1).is_none() {
        return Err(Error::NotFound(format!("object {}", obj.qualified())));
    }
    opt_str_at(&r, 0).ok_or_else(|| {
        Error::Permission(format!(
            "the definition of {} is encrypted or not visible to this login",
            obj.qualified()
        ))
    })
}

async fn object_ddl_on(conn: &mut Conn, obj: &ObjRef, db: &str) -> Result<String> {
    let names = Names { qualify: true };
    match obj.kind {
        ObjectKind::Table | ObjectKind::View => {
            let filter = catalog::one(&obj.schema, &obj.name);
            let key = (obj.schema.clone(), obj.name.clone());
            if let Some((ObjectKind::Index, idx_name)) = &obj.child {
                let idx = catalog::indexes(conn, &filter).await?.remove(&key).unwrap_or_default();
                let i = idx
                    .iter()
                    .find(|i| &i.name == idx_name)
                    .ok_or_else(|| Error::NotFound(format!("index {idx_name}")))?;
                return ddl::create_index(names, &obj.schema, &obj.name, i).ok_or_else(|| {
                    Error::Unsupported("DDL of XML and spatial indexes".into())
                });
            }
            if let Some((ObjectKind::Trigger, trg)) = &obj.child {
                return trigger_definition(conn, &obj.schema, trg).await;
            }
            if obj.kind == ObjectKind::View {
                let mut out = vec![module_definition(conn, obj).await?];
                let idx = catalog::indexes(conn, &filter).await?.remove(&key).unwrap_or_default();
                for i in &idx {
                    if let Some(s) = ddl::create_index(names, &obj.schema, &obj.name, i) {
                        out.push(s);
                    }
                }
                return Ok(out.join("\nGO\n"));
            }
            let cols = catalog::columns(conn, &filter).await?.remove(&key).unwrap_or_default();
            if cols.is_empty() {
                ensure_exists(conn, obj).await?;
            }
            if let Some((ObjectKind::Column, c)) = &obj.child {
                let col = cols
                    .iter()
                    .find(|x| &x.name == c)
                    .ok_or_else(|| Error::NotFound(format!("column {c}")))?;
                return Ok(format!(
                    "ALTER TABLE {} ADD {};",
                    obj.qualified(),
                    ddl::column_definition(col, "")
                ));
            }
            let idx = catalog::indexes(conn, &filter).await?.remove(&key).unwrap_or_default();
            let checks = catalog::checks(conn, &filter).await?.remove(&key).unwrap_or_default();
            let fks = catalog::foreign_keys(conn, &filter).await?.remove(&key).unwrap_or_default();
            let comment = catalog::table_comments(conn, &filter).await?.remove(&key);
            let collation = database_collation(conn).await?;
            let mut out = vec![ddl::create_table(
                names, &obj.schema, &obj.name, &cols, &idx, &checks, &collation,
            )];
            for i in idx.iter().filter(|i| !i.primary && !i.unique_constraint) {
                if let Some(s) = ddl::create_index(names, &obj.schema, &obj.name, i) {
                    out.push(s);
                }
            }
            for f in &fks {
                out.push(ddl::add_foreign_key(names, &obj.schema, &obj.name, f));
            }
            for t in table_triggers(conn, &obj.schema, &obj.name).await? {
                out.push(format!("GO\n{t}"));
            }
            if let Some(c) = comment {
                out.push(ddl::comment_sql(&obj.schema, &obj.name, None, "TABLE", &c));
            }
            for c in cols.iter().filter(|c| c.comment.is_some()) {
                out.push(ddl::comment_sql(
                    &obj.schema,
                    &obj.name,
                    Some(&c.name),
                    "TABLE",
                    c.comment.as_deref().unwrap_or(""),
                ));
            }
            Ok(out.join("\n"))
        }
        ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger => {
            if obj.kind == ObjectKind::Trigger {
                return trigger_definition(conn, &obj.schema, &obj.name).await;
            }
            module_definition(conn, obj).await
        }
        ObjectKind::Sequence => sequence_ddl(conn, obj).await,
        ObjectKind::Synonym => {
            let rows = conn
                .rows(&format!(
                    "SELECT base_object_name FROM sys.synonyms WHERE object_id = OBJECT_ID({})",
                    lit(&obj.qualified())
                ))
                .await?;
            let base = rows
                .first()
                .and_then(|r| opt_str_at(r, 0))
                .ok_or_else(|| Error::NotFound(format!("synonym {}", obj.qualified())))?;
            Ok(format!("CREATE SYNONYM {} FOR {base};", obj.qualified()))
        }
        ObjectKind::Type => type_ddl(conn, obj, db).await,
        other => Err(Error::Unsupported(format!("DDL of {other:?}"))),
    }
}

pub(crate) async fn database_collation(conn: &mut Conn) -> Result<String> {
    let rows = conn
        .rows("SELECT CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS NVARCHAR(128))")
        .await?;
    Ok(rows.first().map(|r| str_at(r, 0)).unwrap_or_default())
}

async fn trigger_definition(conn: &mut Conn, schema: &str, name: &str) -> Result<String> {
    let obj = ObjRef {
        db: None,
        schema: schema.to_string(),
        name: name.to_string(),
        kind: ObjectKind::Trigger,
        child: None,
    };
    module_definition(conn, &obj).await
}

/// Definitions of the triggers of a table.
pub(crate) async fn table_triggers(conn: &mut Conn, schema: &str, table: &str) -> Result<Vec<String>> {
    let rows = conn
        .rows(&format!(
            "SELECT OBJECT_DEFINITION(tr.object_id), tr.is_disabled, SCHEMA_NAME(o.schema_id), tr.name \
             FROM sys.triggers tr JOIN sys.objects o ON o.object_id = tr.object_id \
             WHERE tr.parent_id = OBJECT_ID({}) ORDER BY tr.name",
            lit(&format!("{}.{}", quote(schema), quote(table)))
        ))
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        if let Some(def) = opt_str_at(r, 0) {
            let mut s = def;
            if bool_at(r, 1) {
                s.push_str(&format!(
                    "\nGO\nDISABLE TRIGGER {}.{} ON {}.{};",
                    quote(&str_at(r, 2)),
                    quote(&str_at(r, 3)),
                    quote(schema),
                    quote(table)
                ));
            }
            out.push(s);
        }
    }
    Ok(out)
}

async fn sequence_ddl(conn: &mut Conn, obj: &ObjRef) -> Result<String> {
    let rows = conn
        .rows(&format!(
            "SELECT TYPE_NAME(s.user_type_id), CAST(s.start_value AS NVARCHAR(60)), CAST(s.increment AS NVARCHAR(60)), \
             CAST(s.minimum_value AS NVARCHAR(60)), CAST(s.maximum_value AS NVARCHAR(60)), s.is_cycling, \
             s.is_cached, s.cache_size \
             FROM sys.sequences s WHERE s.object_id = OBJECT_ID({})",
            lit(&obj.qualified())
        ))
        .await?;
    let r = rows
        .first()
        .ok_or_else(|| Error::NotFound(format!("sequence {}", obj.qualified())))?;
    let mut s = format!(
        "CREATE SEQUENCE {} AS {} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {}",
        obj.qualified(),
        str_at(r, 0),
        str_at(r, 1),
        str_at(r, 2),
        str_at(r, 3),
        str_at(r, 4),
        if bool_at(r, 5) { "CYCLE" } else { "NO CYCLE" }
    );
    if bool_at(r, 6) {
        match opt_str_at(r, 7) {
            Some(n) => s.push_str(&format!(" CACHE {n}")),
            None => s.push_str(" CACHE"),
        }
    } else {
        s.push_str(" NO CACHE");
    }
    s.push(';');
    Ok(s)
}

async fn type_ddl(conn: &mut Conn, obj: &ObjRef, db: &str) -> Result<String> {
    let rows = conn
        .rows(&format!(
            "SELECT t.is_table_type, t.is_assembly_type, bt.name, t.max_length, t.precision, t.scale, t.is_nullable, (SELECT tt.type_table_object_id FROM sys.table_types tt WHERE tt.user_type_id = t.user_type_id) \
             FROM sys.types t LEFT JOIN sys.types bt ON bt.user_type_id = t.system_type_id AND bt.is_user_defined = 0 \
             WHERE t.schema_id = SCHEMA_ID({}) AND t.name = {}",
            lit(&obj.schema),
            lit(&obj.name)
        ))
        .await?;
    let r = rows
        .first()
        .ok_or_else(|| Error::NotFound(format!("type {}", obj.qualified())))?;
    if bool_at(r, 1) {
        return Err(Error::Unsupported("DDL of CLR types".into()));
    }
    if bool_at(r, 0) {
        let tt = int_at(r, 7);
        let cols = catalog::columns(conn, &format!("o.object_id = {tt}")).await?;
        let list = cols.into_values().next().unwrap_or_default();
        let collation = database_collation(conn).await?;
        let lines: Vec<String> = list
            .iter()
            .map(|c| ddl::column_definition(c, &collation))
            .collect();
        let _ = db;
        return Ok(format!(
            "CREATE TYPE {} AS TABLE (\n{}\n);",
            obj.qualified(),
            lines.join(",\n")
        ));
    }
    let base = crate::sql::format_type(&str_at(r, 2), int_at(r, 3), int_at(r, 4), int_at(r, 5));
    Ok(format!(
        "CREATE TYPE {} FROM {}{};",
        obj.qualified(),
        base.to_ascii_uppercase(),
        if bool_at(r, 6) { " NULL" } else { " NOT NULL" }
    ))
}

#[allow(dead_code)]
pub(crate) fn schema_node_path(db: &str, schema: &str) -> ObjectPath {
    schema_path(db, schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_resolves_paths() {
        let p = object_path("d", "s", ObjectKind::Table, "t")
            .child(ObjectKind::Folder, folder::COLUMNS);
        let c = context(&p);
        assert_eq!(c.db.as_deref(), Some("d"));
        assert_eq!(c.schema.as_deref(), Some("s"));
        assert_eq!(c.object, Some((ObjectKind::Table, "t".into())));
        assert_eq!(c.last.unwrap().name, folder::COLUMNS);
        let root_roles = ObjectPath::root().child(ObjectKind::Folder, folder::ROLES);
        let c = context(&root_roles);
        assert!(c.db.is_none() && c.object.is_none());
    }

    #[test]
    fn trigger_text() {
        assert_eq!(
            trigger_detail(&["INSERT".into(), "UPDATE".into()], false, true),
            "AFTER INSERT, UPDATE, disabled"
        );
        assert_eq!(trigger_detail(&["DELETE".into()], true, false), "INSTEAD OF DELETE");
    }

    #[test]
    fn function_labels() {
        assert_eq!(function_detail("IF"), "inline table");
        assert_eq!(function_detail("FN "), "scalar");
    }
}
