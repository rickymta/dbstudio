//! DDL reconstruction (`object_ddl`).

use dbstudio_core::{Error, ObjectKind, ObjectPath, Result};
use tokio_postgres::Client;

use crate::error::map_error;
use crate::flavor::Flavor;
use crate::sql::{non_folder_segments, qualified, quote_ident, quote_literal};

/// `SHOW CREATE <what> <qualified name>` (CockroachDB), the text ends with `;`.
async fn show_create(client: &Client, what: &str, qname: &str, missing: Error) -> Result<String> {
    let row = client
        .query_opt(&format!("SHOW CREATE {what} {qname}"), &[])
        .await
        .map_err(|e| match e.as_db_error().map(|d| d.code().code()) {
            // undefined_table / undefined_function / invalid_schema_name
            Some("42P01" | "42883" | "3F000") => missing,
            _ => map_error(e),
        })?
        .ok_or_else(|| Error::NotFound(format!("{what} {qname} not found")))?;
    let text: String = row.get(1);
    let text = text.trim_end().trim_end_matches(';').trim_end();
    Ok(format!("{text};\n"))
}

pub async fn object_ddl(client: &Client, path: &ObjectPath, flavor: Flavor) -> Result<String> {
    use ObjectKind as K;
    let segs = non_folder_segments(path);
    if flavor.is_cockroach() {
        let crdb = match segs.as_slice() {
            [(K::Database, _), (K::Schema, s), (K::Table, t)] => {
                Some(("TABLE", *s, t.to_string(), "table"))
            }
            [(K::Database, _), (K::Schema, s), (K::View, v)] => {
                Some(("TABLE", *s, v.to_string(), "view"))
            }
            [(K::Database, _), (K::Schema, s), (K::Sequence, q)] => {
                Some(("SEQUENCE", *s, q.to_string(), "sequence"))
            }
            [(K::Database, _), (K::Schema, s), (K::Function, f)] => {
                // The tree name is `name(args)`; SHOW CREATE FUNCTION takes the bare name.
                let bare = f.split('(').next().unwrap_or(f).to_string();
                Some(("FUNCTION", *s, bare, "routine"))
            }
            _ => None,
        };
        if let Some((what, schema, name, label)) = crdb {
            return show_create(
                client,
                what,
                &qualified(schema, &name),
                not_found(label, schema, &name),
            )
            .await;
        }
    }
    match segs.as_slice() {
        [(K::Database, _), (K::Schema, s), (K::Table, t)] => table_ddl(client, s, t).await,
        [(K::Database, _), (K::Schema, s), (K::View, v)] => view_ddl(client, s, v, false).await,
        [(K::Database, _), (K::Schema, s), (K::MaterializedView, v)] => {
            view_ddl(client, s, v, true).await
        }
        [
            (K::Database, _),
            (K::Schema, s),
            (kind @ (K::Function | K::Procedure), f),
        ] => function_ddl(client, s, f, *kind).await,
        [(K::Database, _), (K::Schema, s), (K::Sequence, q)] => sequence_ddl(client, s, q).await,
        [
            (K::Database, _),
            (K::Schema, s),
            (K::Table, _),
            (K::Index, i),
        ] => index_ddl(client, s, i).await,
        [
            (K::Database, _),
            (K::Schema, s),
            (K::Table, t),
            (K::Trigger, g),
        ] => trigger_ddl(client, s, t, g).await,
        _ => Err(Error::Unsupported("object_ddl".to_string())),
    }
}

fn not_found(what: &str, schema: &str, name: &str) -> Error {
    Error::NotFound(format!("{what} {schema}.{name} not found"))
}

async fn relation_oid(
    client: &Client,
    schema: &str,
    name: &str,
    relkinds: &str,
) -> Result<Option<u32>> {
    let sql = format!(
        "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ({relkinds})"
    );
    Ok(client
        .query_opt(&sql, &[&schema, &name])
        .await
        .map_err(map_error)?
        .map(|r| r.get(0)))
}

async fn table_ddl(client: &Client, schema: &str, name: &str) -> Result<String> {
    let oid = relation_oid(client, schema, name, "'r', 'p', 'f'")
        .await?
        .ok_or_else(|| not_found("table", schema, name))?;
    let head = client
        .query_one(
            "SELECT c.relkind::text, c.relpersistence::text, c.relispartition, \
                    CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END, \
                    pn.nspname::text, pc.relname::text, pg_get_expr(c.relpartbound, c.oid), \
                    obj_description(c.oid, 'pg_class'), fs.srvname::text, ft.ftoptions::text[] \
             FROM pg_class c \
             LEFT JOIN pg_inherits i ON i.inhrelid = c.oid AND c.relispartition \
             LEFT JOIN pg_class pc ON pc.oid = i.inhparent \
             LEFT JOIN pg_namespace pn ON pn.oid = pc.relnamespace \
             LEFT JOIN pg_foreign_table ft ON ft.ftrelid = c.oid \
             LEFT JOIN pg_foreign_server fs ON fs.oid = ft.ftserver \
             WHERE c.oid = $1",
            &[&oid],
        )
        .await
        .map_err(map_error)?;
    let relkind: String = head.get(0);
    let persistence: String = head.get(1);
    let is_partition: bool = head.get(2);
    let partkey: Option<String> = head.get(3);
    let parent_schema: Option<String> = head.get(4);
    let parent_name: Option<String> = head.get(5);
    let bound: Option<String> = head.get(6);
    let table_comment: Option<String> = head.get(7);
    let server: Option<String> = head.get(8);
    let fdw_options: Option<Vec<String>> = head.get(9);

    let qname = qualified(schema, name);
    let cols = client
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                    pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.attgenerated::text, \
                    col_description(a.attrelid, a.attnum), \
                    CASE WHEN a.attcollation <> t.typcollation THEN \
                         (SELECT quote_ident(cn.nspname) || '.' || quote_ident(co.collname) \
                          FROM pg_collation co JOIN pg_namespace cn ON cn.oid = co.collnamespace \
                          WHERE co.oid = a.attcollation) END \
             FROM pg_attribute a \
             JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(map_error)?;

    let mut out = String::new();
    let mut comments = Vec::new();
    if let Some(c) = &table_comment {
        let kw = if relkind == "f" {
            "FOREIGN TABLE"
        } else {
            "TABLE"
        };
        comments.push(format!("COMMENT ON {kw} {qname} IS {};", quote_literal(c)));
    }
    for r in &cols {
        let comment: Option<String> = r.get(6);
        if let Some(c) = comment {
            comments.push(format!(
                "COMMENT ON COLUMN {qname}.{} IS {};",
                quote_ident(&r.get::<_, String>(0)),
                quote_literal(&c)
            ));
        }
    }

    if is_partition {
        out.push_str(&format!(
            "CREATE TABLE {qname} PARTITION OF {}\n    {};\n",
            qualified(
                parent_schema.as_deref().unwrap_or(schema),
                parent_name.as_deref().unwrap_or("")
            ),
            bound.unwrap_or_default()
        ));
    } else {
        let prefix = match (relkind.as_str(), persistence.as_str()) {
            ("f", _) => "CREATE FOREIGN TABLE",
            (_, "u") => "CREATE UNLOGGED TABLE",
            _ => "CREATE TABLE",
        };
        let mut lines: Vec<String> = Vec::new();
        for r in &cols {
            let col = quote_ident(&r.get::<_, String>(0));
            let ty: String = r.get(1);
            let not_null: bool = r.get(2);
            let default: Option<String> = r.get(3);
            let identity: String = r.get(4);
            let generated: String = r.get(5);
            let collation: Option<String> = r.get(7);
            let mut line = format!("    {col} {ty}");
            if let Some(coll) = collation {
                line.push_str(&format!(" COLLATE {coll}"));
            }
            match (identity.as_str(), generated.as_str(), default) {
                ("a", _, _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                ("d", _, _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                (_, "s", Some(expr)) => {
                    line.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"))
                }
                (_, _, Some(expr)) => line.push_str(&format!(" DEFAULT {expr}")),
                _ => {}
            }
            if not_null {
                line.push_str(" NOT NULL");
            }
            lines.push(line);
        }
        let constraints = client
            .query(
                "SELECT conname::text, pg_get_constraintdef(oid, true) \
                 FROM pg_constraint \
                 WHERE conrelid = $1 AND contype IN ('p', 'u', 'c', 'f', 'x') AND conislocal \
                 ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2 \
                                       WHEN 'f' THEN 3 ELSE 4 END, conname",
                &[&oid],
            )
            .await
            .map_err(map_error)?;
        for r in &constraints {
            lines.push(format!(
                "    CONSTRAINT {} {}",
                quote_ident(&r.get::<_, String>(0)),
                r.get::<_, String>(1)
            ));
        }
        out.push_str(&format!("{prefix} {qname} (\n{}\n)", lines.join(",\n")));
        if let Some(key) = partkey {
            out.push_str(&format!("\nPARTITION BY {key}"));
        }
        if relkind == "f" {
            if let Some(srv) = server {
                out.push_str(&format!("\nSERVER {}", quote_ident(&srv)));
            }
            if let Some(opts) = fdw_options.filter(|o| !o.is_empty()) {
                let rendered: Vec<String> = opts
                    .iter()
                    .map(|o| match o.split_once('=') {
                        Some((k, v)) => format!("{} {}", quote_ident(k), quote_literal(v)),
                        None => quote_ident(o),
                    })
                    .collect();
                out.push_str(&format!("\nOPTIONS ({})", rendered.join(", ")));
            }
        }
        out.push_str(";\n");
    }

    let indexes = standalone_indexes(client, oid).await?;
    for def in indexes {
        out.push_str(&format!("\n{def};"));
    }
    if !comments.is_empty() {
        out.push('\n');
        for c in comments {
            out.push_str(&format!("\n{c}"));
        }
    }
    out.push('\n');
    Ok(out)
}

/// Index definitions that are not created implicitly by a PK/unique/exclusion constraint.
async fn standalone_indexes(client: &Client, table_oid: u32) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
             WHERE i.indrelid = $1 AND NOT EXISTS ( \
                 SELECT 1 FROM pg_constraint c \
                 WHERE c.conindid = i.indexrelid AND c.conrelid = i.indrelid \
                   AND c.contype IN ('p', 'u', 'x')) \
             ORDER BY 1",
            &[&table_oid],
        )
        .await
        .map_err(map_error)?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

async fn view_ddl(client: &Client, schema: &str, name: &str, materialized: bool) -> Result<String> {
    let (relkind, what) = if materialized {
        ("'m'", "materialized view")
    } else {
        ("'v'", "view")
    };
    let oid = relation_oid(client, schema, name, relkind)
        .await?
        .ok_or_else(|| not_found(what, schema, name))?;
    let def: String = client
        .query_one("SELECT pg_get_viewdef($1::oid, true)", &[&oid])
        .await
        .map_err(map_error)?
        .get(0);
    let def = def.trim_end().trim_end_matches(';').trim_end();
    let qname = qualified(schema, name);
    let mut out = if materialized {
        format!("CREATE MATERIALIZED VIEW {qname} AS\n{def};\n")
    } else {
        format!("CREATE OR REPLACE VIEW {qname} AS\n{def};\n")
    };
    if materialized {
        for idx in standalone_indexes(client, oid).await? {
            out.push_str(&format!("\n{idx};"));
        }
    }
    Ok(out)
}

async fn function_ddl(
    client: &Client,
    schema: &str,
    name: &str,
    kind: ObjectKind,
) -> Result<String> {
    let kinds = if kind == ObjectKind::Procedure {
        "p.prokind = 'p'"
    } else {
        "p.prokind IN ('f', 'w')"
    };
    let sql = format!(
        "SELECT pg_get_functiondef(p.oid) FROM pg_proc p \
         JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = $1 AND {kinds} \
           AND p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')' = $2"
    );
    let row = client
        .query_opt(&sql, &[&schema, &name])
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("routine", schema, name))?;
    let def: String = row.get(0);
    Ok(format!("{};\n", def.trim_end()))
}

async fn index_ddl(client: &Client, schema: &str, name: &str) -> Result<String> {
    let row = client
        .query_opt(
            "SELECT pg_get_indexdef(c.oid) FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('i', 'I')",
            &[&schema, &name],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("index", schema, name))?;
    Ok(format!("{};\n", row.get::<_, String>(0)))
}

async fn trigger_ddl(client: &Client, schema: &str, table: &str, name: &str) -> Result<String> {
    let row = client
        .query_opt(
            "SELECT pg_get_triggerdef(t.oid, true) FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND t.tgname = $3 AND NOT t.tgisinternal",
            &[&schema, &table, &name],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("trigger", schema, name))?;
    Ok(format!("{};\n", row.get::<_, String>(0)))
}

async fn sequence_ddl(client: &Client, schema: &str, name: &str) -> Result<String> {
    let row = client
        .query_opt(
            "SELECT format_type(s.seqtypid, NULL), s.seqstart, s.seqincrement, s.seqmin, \
                    s.seqmax, s.seqcache, s.seqcycle, \
                    (SELECT format('%I.%I.%I', tn.nspname, tc.relname, a.attname) \
                     FROM pg_depend d \
                     JOIN pg_class tc ON tc.oid = d.refobjid \
                     JOIN pg_namespace tn ON tn.oid = tc.relnamespace \
                     JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
                     WHERE d.objid = c.oid AND d.classid = 'pg_class'::regclass \
                       AND d.refclassid = 'pg_class'::regclass AND d.deptype = 'a' LIMIT 1) \
             FROM pg_sequence s \
             JOIN pg_class c ON c.oid = s.seqrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &name],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| not_found("sequence", schema, name))?;
    let qname = qualified(schema, name);
    let mut out = format!(
        "CREATE SEQUENCE {qname}\n    AS {}\n    START WITH {}\n    INCREMENT BY {}\n    \
         MINVALUE {}\n    MAXVALUE {}\n    CACHE {}\n    {}CYCLE;\n",
        row.get::<_, String>(0),
        row.get::<_, i64>(1),
        row.get::<_, i64>(2),
        row.get::<_, i64>(3),
        row.get::<_, i64>(4),
        row.get::<_, i64>(5),
        if row.get::<_, bool>(6) { "" } else { "NO " },
    );
    if let Some(owner) = row.get::<_, Option<String>>(7) {
        out.push_str(&format!("\nALTER SEQUENCE {qname} OWNED BY {owner};\n"));
    }
    Ok(out)
}
