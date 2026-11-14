//! PostgreSQL driver: DDL generation. Owned by work package F.
//!
//! The DDL of a database, schema or a selection of objects is built from a fixed number of bulk
//! `pg_catalog` queries (not one per object) inside one read-only snapshot whose `search_path`
//! is `pg_catalog`, so that `pg_get_*def` functions qualify every name.
//!
//! The result is ordered: all `PreData` items first (schemas, types, sequences, tables without
//! foreign keys, views, routines), then all `PostData` items (indexes, foreign keys, triggers,
//! sequence ownership and values, comments, grants). Running the items in order against an
//! empty database recreates the objects; foreign keys never get in the way of table creation
//! order because they all live in `PostData`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use dbstudio_core::{
    DdlItem, DdlLanguage, DdlOptions, DdlRequest, DdlSection, Error, ObjectKind, ObjectPath,
    Result, folder,
};
use tokio_postgres::Row;

use crate::browse::{object_path, schema_path};
use crate::error::map_error;
use crate::model::{catalog_query, parse_scope};
use crate::session::PostgresSession;
use crate::sql::{non_folder_segments, qualified, quote_ident, quote_literal};

/// Ordered DDL of the objects in a scope.
pub(crate) async fn generate(session: &PostgresSession, req: DdlRequest) -> Result<Vec<DdlItem>> {
    let target = Target::parse(&req)?;
    if session.shared.flavor().is_cockroach() {
        return crate::ddl_crdb::generate(session, &req, &target).await;
    }
    let database = target
        .database
        .clone()
        .unwrap_or_else(|| session.login_db().to_string());
    let mut conn = session.shared.acquire_catalog(&database).await?;
    let client = &conn.client;
    client
        .batch_execute(
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL search_path = pg_catalog",
        )
        .await
        .map_err(map_error)?;
    let result = async {
        let cat = Catalog::load(client, &target).await?;
        Ok(render(&database, &target, &req.options, cat))
    }
    .await;
    if client.batch_execute("ROLLBACK").await.is_ok() {
        conn.mark_reusable();
    }
    result
}

// ---------------------------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------------------------

/// Object categories a selection can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Cat {
    Relation,
    Sequence,
    Routine,
    Type,
}

#[derive(Debug)]
pub(crate) struct Target {
    pub(crate) database: Option<String>,
    pub(crate) schema: Option<String>,
    /// `Some` when the request names objects; everything else is left out (except dependencies).
    pub(crate) selection: Option<BTreeSet<(Cat, String, String)>>,
    /// The scope itself is one object (FKs to other tables are kept).
    single: bool,
}

impl Target {
    pub(crate) fn parse(req: &DdlRequest) -> Result<Target> {
        use ObjectKind as K;
        let segs = non_folder_segments(&req.scope);
        let mut selection: BTreeSet<(Cat, String, String)> = BTreeSet::new();
        let mut single = false;
        let (database, schema) = match segs.as_slice() {
            [] | [(K::Database, _)] | [(K::Database, _), (K::Schema, _)] => {
                let s = parse_scope(&req.scope)?;
                (s.database, s.schema)
            }
            [(K::Database, d), (K::Schema, s), (kind, _)] => {
                single = true;
                if !is_object_kind(*kind) {
                    return Err(Error::Unsupported("ddl_generation".into()));
                }
                (Some((*d).to_string()), Some((*s).to_string()))
            }
            _ => return Err(Error::Unsupported("ddl_generation".into())),
        };
        let mut paths: Vec<&ObjectPath> = req.objects.iter().collect();
        if single {
            paths.push(&req.scope);
        }
        for p in paths {
            let segs = non_folder_segments(p);
            if let [(K::Database, _), (K::Schema, s), (kind, name)] = segs.as_slice() {
                if let Some(cat) = cat_of(*kind) {
                    selection.insert((cat, (*s).to_string(), (*name).to_string()));
                }
            }
        }
        if !req.objects.is_empty() || single {
            if selection.is_empty() {
                return Err(Error::NotFound("no object of the request was found".into()));
            }
            // Objects restrict the schemas as well.
            if schema.is_none() {
                let schemas: BTreeSet<&String> = selection.iter().map(|(_, s, _)| s).collect();
                if schemas.len() == 1 {
                    let only = schemas.into_iter().next().cloned();
                    return Ok(Target {
                        database,
                        schema: only,
                        selection: Some(selection),
                        single,
                    });
                }
            }
        }
        Ok(Target {
            database,
            schema,
            selection: if selection.is_empty() {
                None
            } else {
                Some(selection)
            },
            single,
        })
    }

    pub(crate) fn wants(&self, cat: Cat, schema: &str, name: &str) -> bool {
        match &self.selection {
            None => true,
            Some(s) => s.contains(&(cat, schema.to_string(), name.to_string())),
        }
    }
}

fn is_object_kind(k: ObjectKind) -> bool {
    matches!(
        k,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::MaterializedView
            | ObjectKind::Sequence
            | ObjectKind::Function
            | ObjectKind::Procedure
            | ObjectKind::Type
    )
}

fn cat_of(k: ObjectKind) -> Option<Cat> {
    match k {
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => Some(Cat::Relation),
        ObjectKind::Sequence => Some(Cat::Sequence),
        ObjectKind::Function | ObjectKind::Procedure => Some(Cat::Routine),
        ObjectKind::Type => Some(Cat::Type),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Catalog data
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct SchemaRow {
    name: String,
    comment: Option<String>,
}

#[derive(Debug)]
struct RelRow {
    oid: u32,
    schema: String,
    name: String,
    relkind: String,
    unlogged: bool,
    is_partition: bool,
    partkey: Option<String>,
    parent: Option<(String, String)>,
    bound: Option<String>,
    comment: Option<String>,
    view_def: Option<String>,
    options: Vec<String>,
    populated: bool,
    server: Option<String>,
    fdw_options: Vec<String>,
    /// Parents of a classic (non-partition) inheritance.
    inherits: Vec<(String, String)>,
}

#[derive(Debug)]
struct ColRow {
    rel: u32,
    name: String,
    type_name: String,
    not_null: bool,
    default: Option<String>,
    identity: String,
    generated: String,
    comment: Option<String>,
    collation: Option<String>,
}

#[derive(Debug)]
struct ConRow {
    rel: u32,
    name: String,
    def: String,
    contype: String,
    ref_rel: Option<u32>,
}

#[derive(Debug)]
struct IdxRow {
    rel: u32,
    name: String,
    def: String,
}

#[derive(Debug)]
struct TrgRow {
    rel: u32,
    name: String,
    def: String,
}

#[derive(Debug)]
struct SeqRow {
    oid: u32,
    schema: String,
    name: String,
    type_name: String,
    start: i64,
    increment: i64,
    min: i64,
    max: i64,
    cache: i64,
    cycle: bool,
    /// `a` (serial, owned by a column) or `i` (identity).
    owner_dep: Option<String>,
    owner: Option<(String, String, String)>,
    last_value: Option<i64>,
    /// The connected role may read the current value (`SELECT` on the sequence).
    readable: bool,
    comment: Option<String>,
}

#[derive(Debug)]
struct TypeRow {
    schema: String,
    name: String,
    typtype: String,
    comment: Option<String>,
    labels: Vec<String>,
    attrs: Vec<String>,
    base: Option<String>,
    not_null: bool,
    default: Option<String>,
    constraints: Vec<String>,
    /// `SUBTYPE = ..., COLLATION = ...` of a range type.
    range_opts: Option<String>,
}

#[derive(Debug)]
struct FuncRow {
    oid: u32,
    schema: String,
    name: String,
    prokind: String,
    def: String,
    args: String,
    comment: Option<String>,
}

#[derive(Debug)]
struct GrantRow {
    rel: u32,
    grantee: String,
    privilege: String,
    grantable: bool,
}

#[derive(Debug, Default)]
struct Catalog {
    schemas: Vec<SchemaRow>,
    rels: Vec<RelRow>,
    cols: Vec<ColRow>,
    constraints: Vec<ConRow>,
    indexes: Vec<IdxRow>,
    triggers: Vec<TrgRow>,
    seqs: Vec<SeqRow>,
    types: Vec<TypeRow>,
    funcs: Vec<FuncRow>,
    /// `(owner relation oid, referenced oid, referenced is a routine)`.
    deps: Vec<(u32, u32, bool)>,
    grants: Vec<GrantRow>,
    /// `(extension, schema)` of the installed extensions except plpgsql.
    extensions: Vec<(String, String)>,
}

const NOT_EXTENSION: &str = "NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = {oid} \
                             AND d.classid = '{class}'::regclass AND d.deptype = 'e')";

fn not_ext(oid: &str, class: &str) -> String {
    NOT_EXTENSION
        .replace("{oid}", oid)
        .replace("{class}", class)
}

const SCHEMA_FILTER: &str = "{n}.nspname NOT IN ('pg_catalog', 'information_schema') \
                             AND {n}.nspname !~ '^pg_(toast|temp)' \
                             AND ($1::text IS NULL OR {n}.nspname = $1::text)";

fn schema_filter(alias: &str) -> String {
    SCHEMA_FILTER.replace("{n}", alias)
}

impl Catalog {
    async fn load(client: &tokio_postgres::Client, target: &Target) -> Result<Catalog> {
        let mut cat = Catalog::default();
        let schema = target.schema.clone();

        let sql = format!(
            "SELECT n.nspname::text, obj_description(n.oid, 'pg_namespace') \
             FROM pg_namespace n WHERE {} AND {} ORDER BY n.nspname",
            schema_filter("n"),
            not_ext("n.oid", "pg_namespace")
        );
        for r in catalog_query(client, &sql, &[&schema]).await? {
            cat.schemas.push(SchemaRow {
                name: r.get(0),
                comment: r.get(1),
            });
        }

        let sql = format!(
            "SELECT c.oid, n.nspname::text, c.relname::text, c.relkind::text, \
                    c.relpersistence = 'u', c.relispartition, \
                    CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END, \
                    pn.nspname::text, pc.relname::text, pg_get_expr(c.relpartbound, c.oid), \
                    obj_description(c.oid, 'pg_class'), \
                    CASE WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid, true) END, \
                    c.reloptions::text[], \
                    CASE WHEN c.relkind = 'm' THEN c.relispopulated ELSE true END, \
                    fs.srvname::text, ft.ftoptions::text[], \
                    ARRAY(SELECT ihn.nspname::text FROM pg_inherits ih \
                          JOIN pg_class ihc ON ihc.oid = ih.inhparent \
                          JOIN pg_namespace ihn ON ihn.oid = ihc.relnamespace \
                          WHERE ih.inhrelid = c.oid AND NOT c.relispartition \
                          ORDER BY ih.inhseqno)::text[], \
                    ARRAY(SELECT ihc.relname::text FROM pg_inherits ih \
                          JOIN pg_class ihc ON ihc.oid = ih.inhparent \
                          WHERE ih.inhrelid = c.oid AND NOT c.relispartition \
                          ORDER BY ih.inhseqno)::text[] \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_inherits i ON i.inhrelid = c.oid AND c.relispartition \
             LEFT JOIN pg_class pc ON pc.oid = i.inhparent \
             LEFT JOIN pg_namespace pn ON pn.oid = pc.relnamespace \
             LEFT JOIN pg_foreign_table ft ON ft.ftrelid = c.oid \
             LEFT JOIN pg_foreign_server fs ON fs.oid = ft.ftserver \
             WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm') AND {} AND {} \
             ORDER BY n.nspname, c.relname",
            schema_filter("n"),
            not_ext("c.oid", "pg_class")
        );
        for r in catalog_query(client, &sql, &[&schema]).await? {
            let pn: Option<String> = r.get(7);
            let pc: Option<String> = r.get(8);
            cat.rels.push(RelRow {
                oid: r.get(0),
                schema: r.get(1),
                name: r.get(2),
                relkind: r.get(3),
                unlogged: r.get(4),
                is_partition: r.get(5),
                partkey: r.get(6),
                parent: pn.zip(pc),
                bound: r.get(9),
                comment: r.get(10),
                view_def: r.get(11),
                options: r.get::<_, Option<Vec<String>>>(12).unwrap_or_default(),
                populated: r.get(13),
                server: r.get(14),
                fdw_options: r.get::<_, Option<Vec<String>>>(15).unwrap_or_default(),
                inherits: r
                    .get::<_, Option<Vec<String>>>(16)
                    .unwrap_or_default()
                    .into_iter()
                    .zip(r.get::<_, Option<Vec<String>>>(17).unwrap_or_default())
                    .collect(),
            });
        }
        let oids: Vec<u32> = cat.rels.iter().map(|r| r.oid).collect();

        if !oids.is_empty() {
            let sql = "SELECT a.attrelid, a.attname::text, format_type(a.atttypid, a.atttypmod), \
                              a.attnotnull, pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, \
                              a.attgenerated::text, col_description(a.attrelid, a.attnum), \
                              CASE WHEN a.attcollation <> t.typcollation THEN \
                                   (SELECT quote_ident(cn.nspname) || '.' || quote_ident(co.collname) \
                                    FROM pg_collation co \
                                    JOIN pg_namespace cn ON cn.oid = co.collnamespace \
                                    WHERE co.oid = a.attcollation) END \
                       FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
                       LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                       WHERE a.attrelid = ANY($1::oid[]) AND a.attnum > 0 AND NOT a.attisdropped \
                       ORDER BY a.attrelid, a.attnum";
            for r in catalog_query(client, sql, &[&oids]).await? {
                cat.cols.push(ColRow {
                    rel: r.get(0),
                    name: r.get(1),
                    type_name: r.get(2),
                    not_null: r.get(3),
                    default: r.get(4),
                    identity: r.get(5),
                    generated: r.get(6),
                    comment: r.get(7),
                    collation: r.get(8),
                });
            }

            let sql = "SELECT c.conrelid, c.conname::text, pg_get_constraintdef(c.oid, true), \
                              c.contype::text, NULLIF(c.confrelid, 0::oid) \
                       FROM pg_constraint c \
                       WHERE c.conrelid = ANY($1::oid[]) \
                         AND ((c.contype IN ('p', 'u', 'c', 'x') AND c.conislocal) \
                              OR (c.contype = 'f' AND c.conparentid = 0)) \
                       ORDER BY c.conrelid, CASE c.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 \
                                WHEN 'c' THEN 2 WHEN 'x' THEN 3 ELSE 4 END, c.conname";
            for r in catalog_query(client, sql, &[&oids]).await? {
                cat.constraints.push(ConRow {
                    rel: r.get(0),
                    name: r.get(1),
                    def: r.get(2),
                    contype: r.get(3),
                    ref_rel: r.get(4),
                });
            }

            let sql = "SELECT i.indrelid, ic.relname::text, pg_get_indexdef(i.indexrelid) \
                       FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid \
                       WHERE i.indrelid = ANY($1::oid[]) AND NOT ic.relispartition \
                         AND NOT EXISTS (SELECT 1 FROM pg_constraint k \
                                         WHERE k.conindid = i.indexrelid \
                                           AND k.conrelid = i.indrelid \
                                           AND k.contype IN ('p', 'u', 'x')) \
                       ORDER BY i.indrelid, ic.relname";
            for r in catalog_query(client, sql, &[&oids]).await? {
                cat.indexes.push(IdxRow {
                    rel: r.get(0),
                    name: r.get(1),
                    def: r.get(2),
                });
            }

            let sql = "SELECT t.tgrelid, t.tgname::text, pg_get_triggerdef(t.oid, true) \
                       FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                       WHERE t.tgrelid = ANY($1::oid[]) AND NOT t.tgisinternal \
                         AND NOT c.relispartition \
                       ORDER BY t.tgrelid, t.tgname";
            for r in catalog_query(client, sql, &[&oids]).await? {
                cat.triggers.push(TrgRow {
                    rel: r.get(0),
                    name: r.get(1),
                    def: r.get(2),
                });
            }
        }

        let sql = "SELECT e.extname::text, n.nspname::text FROM pg_extension e \
                   JOIN pg_namespace n ON n.oid = e.extnamespace \
                   WHERE e.extname <> 'plpgsql' ORDER BY e.oid";
        for r in catalog_query(client, sql, &[]).await? {
            cat.extensions.push((r.get(0), r.get(1)));
        }

        let sql = format!(
            "SELECT c.oid, n.nspname::text, c.relname::text, format_type(s.seqtypid, NULL), \
                    s.seqstart, s.seqincrement, s.seqmin, s.seqmax, s.seqcache, s.seqcycle, \
                    od.deptype::text, tn.nspname::text, tc.relname::text, ta.attname::text, \
                    CASE WHEN has_sequence_privilege(c.oid, 'SELECT') THEN pg_sequence_last_value(c.oid) END, \
                    obj_description(c.oid, 'pg_class'), has_sequence_privilege(c.oid, 'SELECT') \
             FROM pg_sequence s \
             JOIN pg_class c ON c.oid = s.seqrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_depend od ON od.classid = 'pg_class'::regclass AND od.objid = c.oid \
                  AND od.refclassid = 'pg_class'::regclass AND od.deptype IN ('a', 'i') \
             LEFT JOIN pg_class tc ON tc.oid = od.refobjid \
             LEFT JOIN pg_namespace tn ON tn.oid = tc.relnamespace \
             LEFT JOIN pg_attribute ta ON ta.attrelid = od.refobjid AND ta.attnum = od.refobjsubid \
             WHERE {} AND {} ORDER BY c.oid",
            schema_filter("n"),
            not_ext("c.oid", "pg_class")
        );
        for r in catalog_query(client, &sql, &[&schema]).await? {
            let tn: Option<String> = r.get(11);
            let tc: Option<String> = r.get(12);
            let ta: Option<String> = r.get(13);
            let owner = match (tn, tc, ta) {
                (Some(a), Some(b), Some(c)) => Some((a, b, c)),
                _ => None,
            };
            cat.seqs.push(SeqRow {
                oid: r.get(0),
                schema: r.get(1),
                name: r.get(2),
                type_name: r.get(3),
                start: r.get(4),
                increment: r.get(5),
                min: r.get(6),
                max: r.get(7),
                cache: r.get(8),
                cycle: r.get(9),
                owner_dep: r.get(10),
                owner,
                last_value: r.get(14),
                comment: r.get(15),
                readable: r.get(16),
            });
        }

        let sql = format!(
            "SELECT n.nspname::text, t.typname::text, t.typtype::text, \
                    obj_description(t.oid, 'pg_type'), \
                    CASE WHEN t.typtype = 'e' THEN ARRAY(SELECT e.enumlabel::text \
                         FROM pg_enum e WHERE e.enumtypid = t.oid ORDER BY e.enumsortorder) END, \
                    CASE WHEN t.typtype = 'c' THEN ARRAY(SELECT quote_ident(a.attname) || ' ' \
                         || format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
                         WHERE a.attrelid = t.typrelid AND a.attnum > 0 AND NOT a.attisdropped \
                         ORDER BY a.attnum) END, \
                    CASE WHEN t.typtype = 'd' THEN format_type(t.typbasetype, t.typtypmod) END, \
                    t.typnotnull, pg_get_expr(t.typdefaultbin, 0), \
                    CASE WHEN t.typtype = 'd' THEN ARRAY(SELECT 'CONSTRAINT ' \
                         || quote_ident(k.conname) || ' ' || pg_get_constraintdef(k.oid, true) \
                         FROM pg_constraint k WHERE k.contypid = t.oid ORDER BY k.conname) END, \
                    CASE WHEN t.typtype = 'r' THEN (SELECT 'SUBTYPE = ' \
                         || format_type(rg.rngsubtype, NULL) \
                         || CASE WHEN rg.rngcollation <> 0 THEN ', COLLATION = ' \
                              || (SELECT quote_ident(cn.nspname) || '.' || quote_ident(co.collname) \
                                  FROM pg_collation co \
                                  JOIN pg_namespace cn ON cn.oid = co.collnamespace \
                                  WHERE co.oid = rg.rngcollation) ELSE '' END \
                         FROM pg_range rg WHERE rg.rngtypid = t.oid) END \
             FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE t.typtype IN ('e', 'd', 'c', 'r') AND {} AND {} \
               AND (t.typtype <> 'c' OR EXISTS (SELECT 1 FROM pg_class k \
                    WHERE k.oid = t.typrelid AND k.relkind = 'c')) \
             ORDER BY t.oid",
            schema_filter("n"),
            not_ext("t.oid", "pg_type")
        );
        for r in catalog_query(client, &sql, &[&schema]).await? {
            cat.types.push(TypeRow {
                schema: r.get(0),
                name: r.get(1),
                typtype: r.get(2),
                comment: r.get(3),
                labels: r.get::<_, Option<Vec<String>>>(4).unwrap_or_default(),
                attrs: r.get::<_, Option<Vec<String>>>(5).unwrap_or_default(),
                base: r.get(6),
                not_null: r.get(7),
                default: r.get(8),
                constraints: r.get::<_, Option<Vec<String>>>(9).unwrap_or_default(),
                range_opts: r.get(10),
            });
        }

        let sql = format!(
            "SELECT p.oid, n.nspname::text, p.proname::text, p.prokind::text, \
                    pg_get_functiondef(p.oid), pg_get_function_identity_arguments(p.oid), \
                    obj_description(p.oid, 'pg_proc') \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             JOIN pg_language l ON l.oid = p.prolang \
             WHERE p.prokind IN ('f', 'p', 'w') AND l.lanname NOT IN ('internal', 'c') \
               AND {} AND {} ORDER BY p.oid",
            schema_filter("n"),
            not_ext("p.oid", "pg_proc")
        );
        for r in catalog_query(client, &sql, &[&schema]).await? {
            cat.funcs.push(FuncRow {
                oid: r.get(0),
                schema: r.get(1),
                name: r.get(2),
                prokind: r.get(3),
                def: r.get(4),
                args: r.get(5),
                comment: r.get(6),
            });
        }

        // Dependencies of relations on other relations (views) and on routines (defaults,
        // checks, views): used to order views and to pull routines in front of tables.
        if !oids.is_empty() {
            let sql = "SELECT COALESCE(rw.ev_class, ad.adrelid, co.conrelid), d.refobjid, \
                              d.refclassid = 'pg_proc'::regclass \
                       FROM pg_depend d \
                       LEFT JOIN pg_rewrite rw ON d.classid = 'pg_rewrite'::regclass \
                            AND rw.oid = d.objid \
                       LEFT JOIN pg_attrdef ad ON d.classid = 'pg_attrdef'::regclass \
                            AND ad.oid = d.objid \
                       LEFT JOIN pg_constraint co ON d.classid = 'pg_constraint'::regclass \
                            AND co.oid = d.objid \
                       WHERE d.refclassid IN ('pg_class'::regclass, 'pg_proc'::regclass) \
                         AND d.classid IN ('pg_rewrite'::regclass, 'pg_attrdef'::regclass, \
                                           'pg_constraint'::regclass) \
                         AND COALESCE(rw.ev_class, ad.adrelid, co.conrelid) = ANY($1::oid[]) \
                         AND d.refobjid <> COALESCE(rw.ev_class, ad.adrelid, co.conrelid)";
            for r in catalog_query(client, sql, &[&oids]).await? {
                cat.deps.push((r.get(0), r.get(1), r.get(2)));
            }
        }

        // Privileges of relations and sequences.
        let mut grant_oids = oids.clone();
        grant_oids.extend(cat.seqs.iter().map(|s| s.oid));
        if !grant_oids.is_empty() {
            let sql = "SELECT c.oid, CASE WHEN a.grantee = 0 THEN 'PUBLIC' \
                                    ELSE quote_ident(pg_get_userbyid(a.grantee)) END, \
                              a.privilege_type::text, a.is_grantable \
                       FROM pg_class c, aclexplode(c.relacl) a \
                       WHERE c.oid = ANY($1::oid[]) AND a.grantee <> c.relowner \
                       ORDER BY c.oid, 2, a.privilege_type";
            let rows: Vec<Row> = catalog_query(client, sql, &[&grant_oids]).await?;
            for r in rows {
                cat.grants.push(GrantRow {
                    rel: r.get(0),
                    grantee: r.get(1),
                    privilege: r.get(2),
                    grantable: r.get(3),
                });
            }
        }
        Ok(cat)
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

struct Out<'a> {
    database: &'a str,
    opts: &'a DdlOptions,
    pre: Vec<DdlItem>,
    post: Vec<DdlItem>,
}

impl Out<'_> {
    fn name(&self, schema: &str, name: &str) -> String {
        if self.opts.qualify_schema {
            qualified(schema, name)
        } else {
            quote_ident(name)
        }
    }

    fn pre(&mut self, path: ObjectPath, kind: ObjectKind, sql: String, deps: Vec<ObjectPath>) {
        self.pre
            .push(item(path, kind, DdlSection::PreData, sql, deps));
    }

    fn post(&mut self, path: ObjectPath, kind: ObjectKind, sql: String, deps: Vec<ObjectPath>) {
        self.post
            .push(item(path, kind, DdlSection::PostData, sql, deps));
    }
}

fn item(
    path: ObjectPath,
    kind: ObjectKind,
    section: DdlSection,
    sql: String,
    deps: Vec<ObjectPath>,
) -> DdlItem {
    DdlItem {
        path,
        kind,
        section,
        sql,
        language: DdlLanguage::Sql,
        depends_on: deps,
    }
}

fn rel_kind(relkind: &str) -> ObjectKind {
    match relkind {
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        _ => ObjectKind::Table,
    }
}

fn type_path(db: &str, schema: &str, name: &str) -> ObjectPath {
    schema_path(db, schema)
        .child(ObjectKind::Folder, folder::TYPES)
        .child(ObjectKind::Type, name)
}

fn child_path(table: &ObjectPath, folder_name: &str, kind: ObjectKind, name: &str) -> ObjectPath {
    table
        .child(ObjectKind::Folder, folder_name)
        .child(kind, name)
}

fn with_options(options: &[String]) -> String {
    if options.is_empty() {
        return String::new();
    }
    let rendered: Vec<String> = options
        .iter()
        .map(|o| match o.split_once('=') {
            Some((k, v)) => format!("{k}={}", quote_option_value(v)),
            None => o.clone(),
        })
        .collect();
    format!(" WITH ({})", rendered.join(", "))
}

fn quote_option_value(v: &str) -> String {
    if v.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        v.to_string()
    } else {
        quote_literal(v)
    }
}

/// `CREATE [UNIQUE] INDEX name` -> `CREATE [UNIQUE] INDEX IF NOT EXISTS name`.
fn index_if_not_exists(def: &str) -> String {
    for prefix in ["CREATE UNIQUE INDEX ", "CREATE INDEX "] {
        if let Some(rest) = def.strip_prefix(prefix) {
            return format!("{prefix}IF NOT EXISTS {rest}");
        }
    }
    def.to_string()
}

fn render(database: &str, target: &Target, opts: &DdlOptions, cat: Catalog) -> Vec<DdlItem> {
    let mut out = Out {
        database,
        opts,
        pre: Vec::new(),
        post: Vec::new(),
    };

    // Which relations / sequences / routines are emitted.
    let rels: Vec<&RelRow> = cat
        .rels
        .iter()
        .filter(|r| target.wants(Cat::Relation, &r.schema, &r.name))
        .filter(|r| opts.include_views || !matches!(r.relkind.as_str(), "v" | "m"))
        .collect();
    let rel_oids: HashSet<u32> = rels.iter().map(|r| r.oid).collect();
    let rel_by_oid: HashMap<u32, &RelRow> = cat.rels.iter().map(|r| (r.oid, r)).collect();
    let rel_path = |r: &RelRow| object_path(database, &r.schema, rel_kind(&r.relkind), &r.name);

    // Sequences owned by selected tables (serial) follow their table.
    let seq_selected = |s: &SeqRow| -> bool {
        if target.selection.is_none() {
            return true;
        }
        if target.wants(Cat::Sequence, &s.schema, &s.name) {
            return true;
        }
        match &s.owner {
            Some((sch, tbl, _)) => target.wants(Cat::Relation, sch, tbl),
            None => false,
        }
    };

    // Routines needed by selected relations (defaults, checks, views) are always emitted, in
    // front of the tables; the rest follows the views.
    let needed_funcs: HashSet<u32> = cat
        .deps
        .iter()
        .filter(|(owner, _, is_fn)| *is_fn && rel_oids.contains(owner))
        .map(|(_, f, _)| *f)
        .collect();
    let funcs: Vec<&FuncRow> = cat
        .funcs
        .iter()
        .filter(|f| {
            needed_funcs.contains(&f.oid)
                || (opts.include_routines && target.wants(Cat::Routine, &f.schema, &func_name(f)))
        })
        .collect();

    // --- PreData -------------------------------------------------------------------------
    // Schemas.
    let mut schema_names: BTreeSet<&str> = BTreeSet::new();
    if target.selection.is_none() {
        schema_names.extend(cat.schemas.iter().map(|s| s.name.as_str()));
    } else {
        schema_names.extend(rels.iter().map(|r| r.schema.as_str()));
        schema_names.extend(
            cat.seqs
                .iter()
                .filter(|s| seq_selected(s))
                .map(|s| s.schema.as_str()),
        );
        schema_names.extend(funcs.iter().map(|f| f.schema.as_str()));
    }
    // Session settings as pg_dump writes them: function bodies are not validated while
    // objects are created in dependency order, and names resolve only through qualification.
    {
        let db_path = ObjectPath::root().child(ObjectKind::Database, database);
        let mut header = vec![
            "SET check_function_bodies = false;".to_string(),
            "SET client_encoding = 'UTF8';".to_string(),
            "SET standard_conforming_strings = on;".to_string(),
        ];
        if opts.qualify_schema {
            header.push("SELECT pg_catalog.set_config('search_path', '', false);".to_string());
        }
        for sql in header {
            out.pre(db_path.clone(), ObjectKind::Database, sql, vec![]);
        }
    }
    if opts.qualify_schema {
        for s in cat
            .schemas
            .iter()
            .filter(|s| schema_names.contains(s.name.as_str()))
        {
            if s.name == "public" {
                continue;
            }
            let ine = if opts.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            };
            out.pre(
                schema_path(database, &s.name),
                ObjectKind::Schema,
                format!("CREATE SCHEMA {ine}{};", quote_ident(&s.name)),
                vec![],
            );
        }
    }

    // Extensions (not their member objects) before the types and tables that use them.
    if target.selection.is_none() {
        let db_path = ObjectPath::root().child(ObjectKind::Database, database);
        for (name, schema) in &cat.extensions {
            let with_schema = if opts.qualify_schema
                && (schema == "public" || schema_names.contains(schema.as_str()))
            {
                format!(" WITH SCHEMA {}", quote_ident(schema))
            } else {
                String::new()
            };
            out.pre(
                db_path.clone(),
                ObjectKind::Database,
                format!(
                    "CREATE EXTENSION IF NOT EXISTS {}{with_schema};",
                    quote_ident(name)
                ),
                vec![],
            );
        }
    }

    // Domain constraints that are NOT VALID cannot be part of CREATE DOMAIN.
    let mut domain_post: Vec<(ObjectPath, String)> = Vec::new();

    // Types (oid order keeps dependencies of composite/domain types on earlier types).
    for t in cat.types.iter().filter(|t| {
        target.selection.is_none()
            || target.wants(Cat::Type, &t.schema, &t.name)
            || schema_names.contains(t.schema.as_str())
    }) {
        let q = out.name(&t.schema, &t.name);
        let path = type_path(database, &t.schema, &t.name);
        if opts.include_drop {
            out.pre(
                path.clone(),
                ObjectKind::Type,
                format!("DROP TYPE IF EXISTS {q} CASCADE;"),
                vec![],
            );
        }
        let sql = match t.typtype.as_str() {
            "e" => {
                let labels: Vec<String> = t.labels.iter().map(|l| quote_literal(l)).collect();
                format!("CREATE TYPE {q} AS ENUM ({});", labels.join(", "))
            }
            "c" => format!("CREATE TYPE {q} AS ({});", t.attrs.join(", ")),
            "r" => format!(
                "CREATE TYPE {q} AS RANGE ({});",
                t.range_opts.clone().unwrap_or_default()
            ),
            _ => {
                let mut s = format!(
                    "CREATE DOMAIN {q} AS {}",
                    t.base.clone().unwrap_or_default()
                );
                if let Some(d) = &t.default {
                    s.push_str(&format!(" DEFAULT {d}"));
                }
                if t.not_null {
                    s.push_str(" NOT NULL");
                }
                for c in &t.constraints {
                    if c.ends_with(" NOT VALID") {
                        domain_post.push((path.clone(), format!("ALTER DOMAIN {q} ADD {c};")));
                    } else {
                        s.push_str(&format!(" {c}"));
                    }
                }
                s.push(';');
                s
            }
        };
        out.pre(path, ObjectKind::Type, sql, vec![]);
    }

    // Sequences that are not identity sequences.
    if opts.include_sequences {
        for s in cat
            .seqs
            .iter()
            .filter(|s| s.owner_dep.as_deref() != Some("i") && seq_selected(s))
        {
            let q = out.name(&s.schema, &s.name);
            let path = object_path(database, &s.schema, ObjectKind::Sequence, &s.name);
            if opts.include_drop {
                out.pre(
                    path.clone(),
                    ObjectKind::Sequence,
                    format!("DROP SEQUENCE IF EXISTS {q} CASCADE;"),
                    vec![],
                );
            }
            let ine = if opts.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            };
            out.pre(
                path,
                ObjectKind::Sequence,
                format!(
                    "CREATE SEQUENCE {ine}{q}\n    AS {}\n    START WITH {}\n    INCREMENT BY {}\n    \
                     MINVALUE {}\n    MAXVALUE {}\n    CACHE {}\n    {}CYCLE;",
                    s.type_name,
                    s.start,
                    s.increment,
                    s.min,
                    s.max,
                    s.cache,
                    if s.cycle { "" } else { "NO " }
                ),
                vec![],
            );
        }
    }

    // Routines required by tables and views.
    for f in funcs.iter().filter(|f| needed_funcs.contains(&f.oid)) {
        push_function(&mut out, f);
    }

    // Tables: plain tables first, partitions after their parents.
    let table_rows: Vec<&RelRow> = rels
        .iter()
        .copied()
        .filter(|r| matches!(r.relkind.as_str(), "r" | "p" | "f"))
        .collect();
    let mut cols_by_rel: HashMap<u32, Vec<&ColRow>> = HashMap::new();
    for c in &cat.cols {
        cols_by_rel.entry(c.rel).or_default().push(c);
    }
    let mut cons_by_rel: HashMap<u32, Vec<&ConRow>> = HashMap::new();
    for c in &cat.constraints {
        cons_by_rel.entry(c.rel).or_default().push(c);
    }
    // Identity sequences by owning column: their options belong to the column definition.
    let identity: HashMap<(String, String, String), &SeqRow> = cat
        .seqs
        .iter()
        .filter(|s| s.owner_dep.as_deref() == Some("i"))
        .filter_map(|s| s.owner.clone().map(|o| (o, s)))
        .collect();
    let mut not_valid_checks: Vec<(ObjectPath, String)> = Vec::new();
    let mut emitted_tables: HashSet<(String, String)> = HashSet::new();
    let mut pending: Vec<&RelRow> = table_rows.clone();
    while !pending.is_empty() {
        let before = pending.len();
        let mut next = Vec::new();
        for r in pending {
            let parents: Vec<&(String, String)> = match &r.parent {
                Some(p) if r.is_partition => vec![p],
                _ => r.inherits.iter().collect(),
            };
            let parent_ready = parents.iter().all(|p| {
                emitted_tables.contains(*p)
                    || !table_rows
                        .iter()
                        .any(|t| (&t.schema, &t.name) == (&p.0, &p.1))
            });
            if parent_ready {
                not_valid_checks.extend(push_table(
                    &mut out,
                    r,
                    &cols_by_rel,
                    &cons_by_rel,
                    &identity,
                ));
                emitted_tables.insert((r.schema.clone(), r.name.clone()));
            } else {
                next.push(r);
            }
        }
        pending = next;
        if pending.len() == before {
            // Cycle (cannot happen with real partition trees): emit the rest as they are.
            for r in pending.drain(..) {
                not_valid_checks.extend(push_table(
                    &mut out,
                    r,
                    &cols_by_rel,
                    &cons_by_rel,
                    &identity,
                ));
            }
        }
    }

    // Views and materialized views in dependency order.
    let view_rows: Vec<&RelRow> = rels
        .iter()
        .copied()
        .filter(|r| matches!(r.relkind.as_str(), "v" | "m"))
        .collect();
    let view_oids: HashSet<u32> = view_rows.iter().map(|r| r.oid).collect();
    let mut view_deps: HashMap<u32, Vec<u32>> = HashMap::new();
    for (owner, dep, is_fn) in &cat.deps {
        if !*is_fn && view_oids.contains(owner) && view_oids.contains(dep) {
            view_deps.entry(*owner).or_default().push(*dep);
        }
    }
    let mut done: HashSet<u32> = HashSet::new();
    let mut ordered: Vec<&RelRow> = Vec::new();
    let mut remaining = view_rows.clone();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for v in remaining {
            let ready = view_deps
                .get(&v.oid)
                .is_none_or(|ds| ds.iter().all(|d| done.contains(d)));
            if ready {
                done.insert(v.oid);
                ordered.push(v);
            } else {
                next.push(v);
            }
        }
        remaining = next;
        if remaining.len() == before {
            ordered.append(&mut remaining);
        }
    }
    let mut refreshes: Vec<(ObjectPath, String)> = Vec::new();
    for v in ordered {
        push_view(&mut out, v, &view_deps, &rel_by_oid);
        if v.relkind == "m" && v.populated {
            refreshes.push((
                object_path(database, &v.schema, ObjectKind::MaterializedView, &v.name),
                format!(
                    "REFRESH MATERIALIZED VIEW {};",
                    out.name(&v.schema, &v.name)
                ),
            ));
        }
    }

    // Remaining routines.
    for f in funcs.iter().filter(|f| !needed_funcs.contains(&f.oid)) {
        push_function(&mut out, f);
    }

    // --- PostData ------------------------------------------------------------------------
    if opts.include_indexes {
        for i in &cat.indexes {
            let Some(rel) = rel_by_oid.get(&i.rel).filter(|r| rel_oids.contains(&r.oid)) else {
                continue;
            };
            let tp = rel_path(rel);
            // An index of a partitioned table is created on the parent without ONLY, so that
            // PostgreSQL creates (and attaches) the matching index of every partition itself.
            let base = if rel.relkind == "p" {
                i.def.replacen(" ON ONLY ", " ON ", 1)
            } else {
                i.def.clone()
            };
            let def = if opts.if_not_exists {
                index_if_not_exists(&base)
            } else {
                base
            };
            out.post(
                child_path(&tp, folder::INDEXES, ObjectKind::Index, &i.name),
                ObjectKind::Index,
                format!("{def};"),
                vec![tp],
            );
        }
    }
    if opts.include_foreign_keys {
        for c in cat.constraints.iter().filter(|c| c.contype == "f") {
            let Some(rel) = rel_by_oid.get(&c.rel).filter(|r| rel_oids.contains(&r.oid)) else {
                continue;
            };
            let refrel = c.ref_rel.and_then(|o| rel_by_oid.get(&o)).copied();
            // A foreign key to a table that is not part of an explicit selection would break
            // the restore (unless the request is the DDL of that single table).
            if let Some(rr) = refrel {
                if !target.single && !rel_oids.contains(&rr.oid) {
                    continue;
                }
            }
            let tp = rel_path(rel);
            let mut deps = vec![tp.clone()];
            if let Some(rr) = refrel {
                deps.push(rel_path(rr));
            }
            let kw = if rel.relkind == "f" {
                "FOREIGN TABLE"
            } else {
                "TABLE"
            };
            out.post(
                tp,
                ObjectKind::Table,
                format!(
                    "ALTER {kw} {} ADD CONSTRAINT {} {};",
                    out.name(&rel.schema, &rel.name),
                    quote_ident(&c.name),
                    c.def
                ),
                deps,
            );
        }
    }
    for (path, sql) in domain_post {
        out.post(path, ObjectKind::Type, sql, vec![]);
    }
    for (path, sql) in not_valid_checks {
        out.post(path.clone(), ObjectKind::Table, sql, vec![path]);
    }
    if opts.include_triggers {
        for t in &cat.triggers {
            let Some(rel) = rel_by_oid.get(&t.rel).filter(|r| rel_oids.contains(&r.oid)) else {
                continue;
            };
            let tp = rel_path(rel);
            out.post(
                child_path(&tp, folder::TRIGGERS, ObjectKind::Trigger, &t.name),
                ObjectKind::Trigger,
                format!("{};", t.def),
                vec![tp],
            );
        }
    }
    if opts.include_sequences {
        for s in cat.seqs.iter().filter(|s| seq_selected(s)) {
            let path = object_path(database, &s.schema, ObjectKind::Sequence, &s.name);
            let q = out.name(&s.schema, &s.name);
            let owner_table = s.owner.as_ref().map(|(sch, tbl, col)| {
                (
                    qualify_pair(&out, sch, tbl),
                    quote_ident(col),
                    (sch.clone(), tbl.clone()),
                    col.clone(),
                )
            });
            match (s.owner_dep.as_deref(), &owner_table) {
                (Some("a"), Some((tbl, col, key, _))) if emitted_table(&rels, key) => out.post(
                    path.clone(),
                    ObjectKind::Sequence,
                    format!("ALTER SEQUENCE {q} OWNED BY {tbl}.{col};"),
                    vec![],
                ),
                _ => {}
            }
            if !s.readable {
                out.post(
                    path.clone(),
                    ObjectKind::Sequence,
                    format!(
                        "-- Warning: no SELECT privilege on sequence {q}: its current value was not captured"
                    ),
                    vec![],
                );
            }
            if let Some(v) = s.last_value {
                let sql = match (s.owner_dep.as_deref(), &owner_table) {
                    (Some("i"), Some((tbl, _, key, raw))) if emitted_table(&rels, key) => format!(
                        "SELECT pg_catalog.setval(pg_catalog.pg_get_serial_sequence({}, {}), {v}, true);",
                        quote_literal(tbl),
                        quote_literal(raw)
                    ),
                    (Some("i"), _) => continue,
                    _ => format!(
                        "SELECT pg_catalog.setval({}, {v}, true);",
                        quote_literal(&q)
                    ),
                };
                out.post(path, ObjectKind::Sequence, sql, vec![]);
            }
        }
    }
    // Materialized views are filled once the data is in.
    for (path, sql) in refreshes {
        out.post(path, ObjectKind::MaterializedView, sql, vec![]);
    }
    if opts.include_comments {
        push_comments(
            &mut out,
            &cat,
            target,
            &rels,
            &funcs,
            &seq_selected,
            &schema_names,
        );
    }
    if opts.include_grants {
        push_grants(&mut out, &cat, &rels, &seq_selected);
    }

    let mut items = out.pre;
    items.extend(out.post);
    items
}

fn emitted_table(rels: &[&RelRow], key: &(String, String)) -> bool {
    rels.iter().any(|r| r.schema == key.0 && r.name == key.1)
}

fn qualify_pair(out: &Out<'_>, schema: &str, name: &str) -> String {
    out.name(schema, name)
}

fn func_name(f: &FuncRow) -> String {
    format!("{}({})", f.name, f.args)
}

fn push_function(out: &mut Out<'_>, f: &FuncRow) {
    let kind = if f.prokind == "p" {
        ObjectKind::Procedure
    } else {
        ObjectKind::Function
    };
    let path = object_path(out.database, &f.schema, kind, &func_name(f));
    if out.opts.include_drop {
        let kw = if f.prokind == "p" {
            "PROCEDURE"
        } else {
            "FUNCTION"
        };
        let q = out.name(&f.schema, &f.name);
        out.pre(
            path.clone(),
            kind,
            format!("DROP {kw} IF EXISTS {q}({});", f.args),
            vec![],
        );
    }
    out.pre(path, kind, format!("{};", f.def.trim_end()), vec![]);
}

fn push_table(
    out: &mut Out<'_>,
    r: &RelRow,
    cols_by_rel: &HashMap<u32, Vec<&ColRow>>,
    cons_by_rel: &HashMap<u32, Vec<&ConRow>>,
    identity: &HashMap<(String, String, String), &SeqRow>,
) -> Vec<(ObjectPath, String)> {
    let q = out.name(&r.schema, &r.name);
    let path = object_path(out.database, &r.schema, ObjectKind::Table, &r.name);
    let foreign = r.relkind == "f";
    if out.opts.include_drop {
        let kw = if foreign { "FOREIGN TABLE" } else { "TABLE" };
        out.pre(
            path.clone(),
            ObjectKind::Table,
            format!("DROP {kw} IF EXISTS {q} CASCADE;"),
            vec![],
        );
    }
    let ine = if out.opts.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    // Constraints other than foreign keys. A NOT VALID check would be validated against the
    // data while it is loaded, so it is added after the data (PostData) instead.
    let mut deferred: Vec<(ObjectPath, String)> = Vec::new();
    let mut constraint_lines: Vec<String> = Vec::new();
    for c in cons_by_rel.get(&r.oid).into_iter().flatten() {
        if c.contype == "f" || (foreign && c.contype != "c") {
            continue;
        }
        if c.def.ends_with(" NOT VALID") {
            let kw = if foreign { "FOREIGN TABLE" } else { "TABLE" };
            deferred.push((
                path.clone(),
                format!(
                    "ALTER {kw} {q} ADD CONSTRAINT {} {};",
                    quote_ident(&c.name),
                    c.def
                ),
            ));
        } else {
            constraint_lines.push(format!("    CONSTRAINT {} {}", quote_ident(&c.name), c.def));
        }
    }
    let mut sql;
    if let (true, Some((ps, pn))) = (r.is_partition, &r.parent) {
        sql = format!("CREATE TABLE {ine}{q} PARTITION OF {}", out.name(ps, pn));
        if !constraint_lines.is_empty() {
            sql.push_str(&format!(" (\n{}\n)", constraint_lines.join(",\n")));
        }
        sql.push_str(&format!("\n    {}", r.bound.clone().unwrap_or_default()));
        if let Some(key) = &r.partkey {
            sql.push_str(&format!("\n    PARTITION BY {key}"));
        }
    } else {
        let prefix = if foreign {
            "CREATE FOREIGN TABLE"
        } else if r.unlogged {
            "CREATE UNLOGGED TABLE"
        } else {
            "CREATE TABLE"
        };
        let mut lines: Vec<String> = cols_by_rel
            .get(&r.oid)
            .map(|v| {
                v.iter()
                    .map(|c| {
                        let seq = identity.get(&(r.schema.clone(), r.name.clone(), c.name.clone()));
                        column_line(c, seq.copied())
                    })
                    .collect()
            })
            .unwrap_or_default();
        lines.extend(constraint_lines);
        sql = format!("{prefix} {ine}{q} (\n{}\n)", lines.join(",\n"));
        if !r.inherits.is_empty() {
            let parents: Vec<String> = r.inherits.iter().map(|(s, n)| out.name(s, n)).collect();
            sql.push_str(&format!("\nINHERITS ({})", parents.join(", ")));
        }
        if let Some(key) = &r.partkey {
            sql.push_str(&format!("\nPARTITION BY {key}"));
        }
        if foreign {
            if let Some(srv) = &r.server {
                sql.push_str(&format!("\nSERVER {}", quote_ident(srv)));
            }
            if !r.fdw_options.is_empty() {
                let rendered: Vec<String> = r
                    .fdw_options
                    .iter()
                    .map(|o| match o.split_once('=') {
                        Some((k, v)) => format!("{} {}", quote_ident(k), quote_literal(v)),
                        None => quote_ident(o),
                    })
                    .collect();
                sql.push_str(&format!("\nOPTIONS ({})", rendered.join(", ")));
            }
        }
    }
    if !foreign {
        sql.push_str(&with_options(&r.options));
    }
    sql.push(';');
    let deps = r
        .inherits
        .iter()
        .map(|(s, n)| object_path(out.database, s, ObjectKind::Table, n))
        .collect();
    out.pre(path, ObjectKind::Table, sql, deps);
    deferred
}

fn column_line(c: &ColRow, identity_seq: Option<&SeqRow>) -> String {
    let mut line = format!("    {} {}", quote_ident(&c.name), c.type_name);
    if let Some(coll) = &c.collation {
        line.push_str(&format!(" COLLATE {coll}"));
    }
    match (c.identity.as_str(), c.generated.as_str(), &c.default) {
        ("a" | "d", _, _) => {
            let always = if c.identity == "a" {
                "ALWAYS"
            } else {
                "BY DEFAULT"
            };
            line.push_str(&format!(" GENERATED {always} AS IDENTITY"));
            if let Some(s) = identity_seq {
                line.push_str(&format!(
                    " (START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {} {}CYCLE)",
                    s.start,
                    s.increment,
                    s.min,
                    s.max,
                    s.cache,
                    if s.cycle { "" } else { "NO " }
                ));
            }
        }
        (_, "s", Some(expr)) => line.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED")),
        (_, "v", Some(expr)) => line.push_str(&format!(" GENERATED ALWAYS AS ({expr}) VIRTUAL")),
        (_, _, Some(expr)) => line.push_str(&format!(" DEFAULT {expr}")),
        _ => {}
    }
    if c.not_null {
        line.push_str(" NOT NULL");
    }
    line
}

fn push_view(
    out: &mut Out<'_>,
    v: &RelRow,
    view_deps: &HashMap<u32, Vec<u32>>,
    rel_by_oid: &HashMap<u32, &RelRow>,
) {
    let materialized = v.relkind == "m";
    let kind = rel_kind(&v.relkind);
    let q = out.name(&v.schema, &v.name);
    let path = object_path(out.database, &v.schema, kind, &v.name);
    let def = v
        .view_def
        .clone()
        .unwrap_or_default()
        .trim_end()
        .trim_end_matches(';')
        .trim_end()
        .to_string();
    let deps: Vec<ObjectPath> = view_deps
        .get(&v.oid)
        .map(|ds| {
            ds.iter()
                .filter_map(|d| rel_by_oid.get(d))
                .map(|r| object_path(out.database, &r.schema, rel_kind(&r.relkind), &r.name))
                .collect()
        })
        .unwrap_or_default();
    if out.opts.include_drop {
        let kw = if materialized {
            "MATERIALIZED VIEW"
        } else {
            "VIEW"
        };
        out.pre(
            path.clone(),
            kind,
            format!("DROP {kw} IF EXISTS {q} CASCADE;"),
            vec![],
        );
    }
    let opts = with_options(&v.options);
    let sql = if materialized {
        let ine = if out.opts.if_not_exists {
            "IF NOT EXISTS "
        } else {
            ""
        };
        // Always created empty; `REFRESH` in PostData fills it after the data load.
        format!("CREATE MATERIALIZED VIEW {ine}{q}{opts} AS\n{def}\nWITH NO DATA;")
    } else {
        format!("CREATE OR REPLACE VIEW {q}{opts} AS\n{def};")
    };
    out.pre(path, kind, sql, deps);
}

fn comment_sql(target_sql: &str, comment: &str) -> String {
    format!("COMMENT ON {target_sql} IS {};", quote_literal(comment))
}

fn push_comments(
    out: &mut Out<'_>,
    cat: &Catalog,
    target: &Target,
    rels: &[&RelRow],
    funcs: &[&FuncRow],
    seq_selected: &dyn Fn(&SeqRow) -> bool,
    schema_names: &BTreeSet<&str>,
) {
    let db = out.database;
    let _ = target;
    for s in &cat.schemas {
        if let (Some(c), true) = (&s.comment, schema_names.contains(s.name.as_str())) {
            out.post(
                schema_path(db, &s.name),
                ObjectKind::Schema,
                comment_sql(&format!("SCHEMA {}", quote_ident(&s.name)), c),
                vec![],
            );
        }
    }
    for t in cat.types.iter().filter(|t| {
        target.selection.is_none()
            || target.wants(Cat::Type, &t.schema, &t.name)
            || schema_names.contains(t.schema.as_str())
    }) {
        if let Some(c) = &t.comment {
            let kw = if t.typtype == "d" { "DOMAIN" } else { "TYPE" };
            out.post(
                type_path(db, &t.schema, &t.name),
                ObjectKind::Type,
                comment_sql(&format!("{kw} {}", out.name(&t.schema, &t.name)), c),
                vec![],
            );
        }
    }
    let mut cols: HashMap<u32, Vec<&ColRow>> = HashMap::new();
    for c in &cat.cols {
        cols.entry(c.rel).or_default().push(c);
    }
    for r in rels {
        let kind = rel_kind(&r.relkind);
        let path = object_path(db, &r.schema, kind, &r.name);
        let q = out.name(&r.schema, &r.name);
        let kw = match r.relkind.as_str() {
            "v" => "VIEW",
            "m" => "MATERIALIZED VIEW",
            "f" => "FOREIGN TABLE",
            _ => "TABLE",
        };
        if let Some(c) = &r.comment {
            out.post(
                path.clone(),
                kind,
                comment_sql(&format!("{kw} {q}"), c),
                vec![],
            );
        }
        for c in cols.get(&r.oid).into_iter().flatten() {
            // Columns of partitions are inherited; their comments live on the parent.
            if let Some(text) = &c.comment {
                out.post(
                    path.clone(),
                    kind,
                    comment_sql(&format!("COLUMN {q}.{}", quote_ident(&c.name)), text),
                    vec![],
                );
            }
        }
    }
    for s in cat.seqs.iter().filter(|s| seq_selected(s)) {
        if let (Some(c), true) = (&s.comment, s.owner_dep.as_deref() != Some("i")) {
            out.post(
                object_path(db, &s.schema, ObjectKind::Sequence, &s.name),
                ObjectKind::Sequence,
                comment_sql(&format!("SEQUENCE {}", out.name(&s.schema, &s.name)), c),
                vec![],
            );
        }
    }
    for f in funcs {
        if let Some(c) = &f.comment {
            let (kw, kind) = if f.prokind == "p" {
                ("PROCEDURE", ObjectKind::Procedure)
            } else {
                ("FUNCTION", ObjectKind::Function)
            };
            out.post(
                object_path(db, &f.schema, kind, &func_name(f)),
                kind,
                comment_sql(
                    &format!("{kw} {}({})", out.name(&f.schema, &f.name), f.args),
                    c,
                ),
                vec![],
            );
        }
    }
}

fn push_grants(
    out: &mut Out<'_>,
    cat: &Catalog,
    rels: &[&RelRow],
    seq_selected: &dyn Fn(&SeqRow) -> bool,
) {
    let db = out.database;
    // (path, kind, "TABLE"/"SEQUENCE", qualified name) per oid.
    let mut targets: HashMap<u32, (ObjectPath, ObjectKind, &'static str, String)> = HashMap::new();
    for r in rels {
        let kind = rel_kind(&r.relkind);
        targets.insert(
            r.oid,
            (
                object_path(db, &r.schema, kind, &r.name),
                kind,
                "TABLE",
                out.name(&r.schema, &r.name),
            ),
        );
    }
    for s in cat.seqs.iter().filter(|s| seq_selected(s)) {
        targets.insert(
            s.oid,
            (
                object_path(db, &s.schema, ObjectKind::Sequence, &s.name),
                ObjectKind::Sequence,
                "SEQUENCE",
                out.name(&s.schema, &s.name),
            ),
        );
    }
    // Group privileges per object and grantee, keeping the catalog order.
    let mut grouped: BTreeMap<(u32, String, bool), Vec<&str>> = BTreeMap::new();
    for g in &cat.grants {
        grouped
            .entry((g.rel, g.grantee.clone(), g.grantable))
            .or_default()
            .push(g.privilege.as_str());
    }
    for ((oid, grantee, grantable), privs) in grouped {
        let Some((path, kind, kw, q)) = targets.get(&oid) else {
            continue;
        };
        let option = if grantable { " WITH GRANT OPTION" } else { "" };
        out.post(
            path.clone(),
            *kind,
            format!(
                "GRANT {} ON {kw} {q} TO {grantee}{option};",
                privs.join(", ")
            ),
            vec![],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_if_not_exists_rewrites_prefix() {
        assert_eq!(
            index_if_not_exists("CREATE UNIQUE INDEX i ON public.t USING btree (a)"),
            "CREATE UNIQUE INDEX IF NOT EXISTS i ON public.t USING btree (a)"
        );
        assert_eq!(
            index_if_not_exists("CREATE INDEX i ON public.t USING gin (a)"),
            "CREATE INDEX IF NOT EXISTS i ON public.t USING gin (a)"
        );
    }

    #[test]
    fn column_lines() {
        let c = ColRow {
            rel: 1,
            name: "id".into(),
            type_name: "integer".into(),
            not_null: true,
            default: None,
            identity: "d".into(),
            generated: String::new(),
            comment: None,
            collation: None,
        };
        assert_eq!(
            column_line(&c, None),
            "    \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL"
        );
        let d = ColRow {
            default: Some("now()".into()),
            identity: String::new(),
            not_null: false,
            name: "at".into(),
            type_name: "timestamp with time zone".into(),
            ..c
        };
        assert_eq!(
            column_line(&d, None),
            "    \"at\" timestamp with time zone DEFAULT now()"
        );
    }

    #[test]
    fn option_rendering() {
        assert_eq!(
            with_options(&["fillfactor=70".to_string()]),
            " WITH (fillfactor=70)"
        );
        assert_eq!(with_options(&[]), "");
    }

    #[test]
    fn target_parsing() {
        let db = ObjectPath::root().child(ObjectKind::Database, "d");
        let req = DdlRequest {
            scope: db.clone(),
            objects: vec![],
            options: DdlOptions::default(),
        };
        let t = Target::parse(&req).unwrap();
        assert!(t.selection.is_none() && !t.single);
        let table = object_path("d", "s", ObjectKind::Table, "t");
        let req = DdlRequest {
            scope: table,
            objects: vec![],
            options: DdlOptions::default(),
        };
        let t = Target::parse(&req).unwrap();
        assert!(t.single && t.schema.as_deref() == Some("s"));
        assert!(t.wants(Cat::Relation, "s", "t") && !t.wants(Cat::Relation, "s", "u"));
    }
}
