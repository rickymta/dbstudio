//! DDL generation from the schema snapshot (`CREATE ...` statements in dependency order).

use std::collections::BTreeSet;

use dbstudio_core::{
    DdlItem, DdlLanguage, DdlOptions, DdlSection, Error, ObjectKind, ObjectPath, Result, folder,
};

use crate::ident::{qualified, quote_ident, quote_text};
use crate::schema::{
    AggregateInfo, ColKind, FunctionInfo, IndexInfo, KeyspaceInfo, Snapshot, TableInfo, UdtInfo,
};

/// `ks.name` or just `name` depending on `qualify`.
fn name_of(ks: &str, name: &str, qualify: bool) -> String {
    if qualify {
        qualified(ks, name)
    } else {
        quote_ident(name)
    }
}

fn ine(if_not_exists: bool) -> &'static str {
    if if_not_exists { " IF NOT EXISTS" } else { "" }
}

pub(crate) fn keyspace_ddl(k: &KeyspaceInfo, if_not_exists: bool) -> String {
    let mut repl: Vec<(&String, &String)> = k.replication.iter().collect();
    repl.sort_by_key(|(key, _)| (key.as_str() != "class", key.as_str()));
    let repl = repl
        .iter()
        .map(|(a, b)| format!("{}: {}", quote_text(a), quote_text(b)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!(
        "CREATE KEYSPACE{} {} WITH replication = {{{repl}}} AND durable_writes = {}",
        ine(if_not_exists),
        quote_ident(&k.name),
        k.durable_writes
    );
    if let Some(t) = k.tablets {
        s.push_str(&format!(" AND tablets = {{'enabled': {t}}}"));
    }
    s.push(';');
    s
}

pub(crate) fn udt_ddl(ks: &str, u: &UdtInfo, qualify: bool, if_not_exists: bool) -> String {
    let fields = u
        .fields
        .iter()
        .map(|(n, t)| format!("    {} {t}", quote_ident(n)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "CREATE TYPE{} {} (\n{fields}\n);",
        ine(if_not_exists),
        name_of(ks, &u.name, qualify)
    )
}

fn primary_key(t: &TableInfo) -> String {
    let parts: Vec<String> = t
        .partition_keys()
        .iter()
        .map(|c| quote_ident(&c.name))
        .collect();
    let partition = if parts.len() == 1 {
        parts[0].clone()
    } else {
        format!("({})", parts.join(", "))
    };
    let mut all = vec![partition];
    all.extend(t.clustering_keys().iter().map(|c| quote_ident(&c.name)));
    format!("PRIMARY KEY ({})", all.join(", "))
}

fn table_options(t: &TableInfo) -> Vec<String> {
    let mut opts = Vec::new();
    let clustering = t.clustering_keys();
    if !clustering.is_empty() {
        opts.push(format!(
            "CLUSTERING ORDER BY ({})",
            clustering
                .iter()
                .map(|c| format!(
                    "{} {}",
                    quote_ident(&c.name),
                    if c.descending { "DESC" } else { "ASC" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for (name, lit) in &t.options {
        opts.push(format!("{name} = {lit}"));
    }
    opts
}

fn with_clause(opts: &[String]) -> String {
    if opts.is_empty() {
        String::new()
    } else {
        format!("\n    WITH {}", opts.join("\n    AND "))
    }
}

pub(crate) fn table_ddl(ks: &str, t: &TableInfo, qualify: bool, if_not_exists: bool) -> String {
    let mut lines: Vec<String> = t
        .columns
        .iter()
        .map(|c| {
            format!(
                "    {} {}{}",
                quote_ident(&c.name),
                c.ty,
                if c.kind == ColKind::Static {
                    " STATIC"
                } else {
                    ""
                }
            )
        })
        .collect();
    lines.push(format!("    {}", primary_key(t)));
    format!(
        "CREATE TABLE{} {} (\n{}\n){};",
        ine(if_not_exists),
        name_of(ks, &t.name, qualify),
        lines.join(",\n"),
        with_clause(&table_options(t))
    )
}

fn index_target(target: &str) -> String {
    for wrap in ["keys", "values", "entries", "full"] {
        if let Some(inner) = target
            .strip_prefix(wrap)
            .and_then(|r| r.strip_prefix('('))
            .and_then(|r| r.strip_suffix(')'))
        {
            return format!("{wrap}({})", quote_ident(inner));
        }
    }
    if target.contains('(') || target.contains(',') {
        target.to_string()
    } else {
        quote_ident(target)
    }
}

pub(crate) fn index_ddl(
    ks: &str,
    table: &str,
    i: &IndexInfo,
    qualify: bool,
    if_not_exists: bool,
) -> String {
    let on = format!(
        "{} ON {} ({})",
        quote_ident(&i.name),
        name_of(ks, table, qualify),
        index_target(&i.target)
    );
    match &i.class {
        Some(class) if i.kind.eq_ignore_ascii_case("CUSTOM") => {
            let mut s = format!(
                "CREATE CUSTOM INDEX{} {on} USING {}",
                ine(if_not_exists),
                quote_text(class)
            );
            let opts: Vec<String> = i
                .options
                .iter()
                .filter(|(k, _)| k.as_str() != "target" && k.as_str() != "class_name")
                .map(|(k, v)| format!("{}: {}", quote_text(k), quote_text(v)))
                .collect();
            if !opts.is_empty() {
                s.push_str(&format!(" WITH OPTIONS = {{{}}}", opts.join(", ")));
            }
            s.push(';');
            s
        }
        _ => format!("CREATE INDEX{} {on};", ine(if_not_exists)),
    }
}

pub(crate) fn view_ddl(ks: &str, v: &TableInfo, qualify: bool, if_not_exists: bool) -> String {
    let cols = if v.include_all_columns {
        "*".to_string()
    } else {
        v.columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let base = v.base_table.clone().unwrap_or_default();
    format!(
        "CREATE MATERIALIZED VIEW{} {} AS\n    SELECT {cols}\n    FROM {}\n    WHERE {}\n    {}{};",
        ine(if_not_exists),
        name_of(ks, &v.name, qualify),
        name_of(ks, &base, qualify),
        v.where_clause.clone().unwrap_or_default(),
        primary_key(v),
        with_clause(&table_options(v))
    )
}

fn dollar_quote(body: &str) -> String {
    if body.contains("$$") {
        quote_text(body)
    } else {
        format!("$${body}$$")
    }
}

pub(crate) fn function_ddl(
    ks: &str,
    f: &FunctionInfo,
    qualify: bool,
    if_not_exists: bool,
) -> String {
    let args = f
        .arg_names
        .iter()
        .zip(&f.arg_types)
        .map(|(n, t)| format!("{} {t}", quote_ident(n)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE FUNCTION{} {} ({args})\n    {} ON NULL INPUT\n    RETURNS {}\n    LANGUAGE {}\n    AS {};",
        ine(if_not_exists),
        name_of(ks, &f.name, qualify),
        if f.called_on_null {
            "CALLED"
        } else {
            "RETURNS NULL"
        },
        f.return_type,
        f.language,
        dollar_quote(&f.body)
    )
}

pub(crate) fn aggregate_ddl(
    ks: &str,
    a: &AggregateInfo,
    qualify: bool,
    if_not_exists: bool,
) -> String {
    let mut s = format!(
        "CREATE AGGREGATE{} {} ({})\n    SFUNC {}\n    STYPE {}",
        ine(if_not_exists),
        name_of(ks, &a.name, qualify),
        a.arg_types.join(", "),
        quote_ident(&a.state_func),
        a.state_type
    );
    if let Some(f) = &a.final_func {
        s.push_str(&format!("\n    FINALFUNC {}", quote_ident(f)));
    }
    if let Some(i) = &a.initcond {
        s.push_str(&format!("\n    INITCOND {i}"));
    }
    s.push(';');
    s
}

pub(crate) fn trigger_ddl(ks: &str, table: &str, name: &str, class: &str, qualify: bool) -> String {
    format!(
        "CREATE TRIGGER {} ON {} USING {};",
        quote_ident(name),
        name_of(ks, table, qualify),
        quote_text(class)
    )
}

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

pub(crate) fn ks_path(ks: &str) -> ObjectPath {
    ObjectPath::root().child(ObjectKind::Database, ks)
}

pub(crate) fn table_path(ks: &str, name: &str, view: bool) -> ObjectPath {
    if view {
        ks_path(ks)
            .child(ObjectKind::Folder, folder::MATERIALIZED_VIEWS)
            .child(ObjectKind::MaterializedView, name)
    } else {
        ks_path(ks)
            .child(ObjectKind::Folder, folder::TABLES)
            .child(ObjectKind::Table, name)
    }
}

pub(crate) fn index_path(ks: &str, table: &TableInfo, index: &str) -> ObjectPath {
    table_path(ks, &table.name, table.is_view)
        .child(ObjectKind::Folder, folder::INDEXES)
        .child(ObjectKind::Index, index)
}

pub(crate) fn type_path(ks: &str, name: &str) -> ObjectPath {
    ks_path(ks)
        .child(ObjectKind::Folder, folder::TYPES)
        .child(ObjectKind::Type, name)
}

pub(crate) fn function_path(ks: &str, signature: &str) -> ObjectPath {
    ks_path(ks)
        .child(ObjectKind::Folder, folder::FUNCTIONS)
        .child(ObjectKind::Function, signature)
}

pub(crate) fn aggregate_path(ks: &str, signature: &str) -> ObjectPath {
    ks_path(ks)
        .child(ObjectKind::Folder, folder::AGGREGATES)
        .child(ObjectKind::Function, signature)
}

pub(crate) fn trigger_path(ks: &str, name: &str) -> ObjectPath {
    ks_path(ks)
        .child(ObjectKind::Folder, folder::TRIGGERS)
        .child(ObjectKind::Trigger, name)
}

// ---------------------------------------------------------------------------------------------
// Whole scope
// ---------------------------------------------------------------------------------------------

/// User-defined types in creation order (a type after the types it uses).
fn ordered_types<'a>(k: &'a KeyspaceInfo, wanted: Option<&BTreeSet<String>>) -> Vec<&'a UdtInfo> {
    let mut order: Vec<&UdtInfo> = Vec::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    fn visit<'a>(
        k: &'a KeyspaceInfo,
        name: &str,
        done: &mut BTreeSet<String>,
        order: &mut Vec<&'a UdtInfo>,
    ) {
        if !done.insert(name.to_string()) {
            return;
        }
        if let Some(u) = k.types.get(name) {
            let mut deps = Vec::new();
            for (_, t) in &u.fields {
                t.udt_names(&mut deps);
            }
            for d in deps {
                visit(k, &d, done, order);
            }
            order.push(u);
        }
    }
    for name in k.types.keys() {
        if wanted.is_none_or(|w| w.contains(name)) {
            visit(k, name, &mut done, &mut order);
        }
    }
    order
}

fn types_used_by(k: &KeyspaceInfo, tables: &[&TableInfo]) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    for t in tables {
        for c in &t.columns {
            c.ty.udt_names(&mut stack);
        }
    }
    while let Some(n) = stack.pop() {
        if found.insert(n.clone()) {
            if let Some(u) = k.types.get(&n) {
                for (_, t) in &u.fields {
                    t.udt_names(&mut stack);
                }
            }
        }
    }
    found
}

/// DDL of the keyspaces in `scope` (`None` = every user keyspace). `only_tables` restricts the
/// output to those tables plus their indexes, triggers and the types they use.
pub(crate) fn generate(
    snap: &Snapshot,
    scope: Option<&str>,
    only_tables: &[(String, String)],
    opts: &DdlOptions,
) -> Result<Vec<DdlItem>> {
    let keyspaces: Vec<&KeyspaceInfo> = match scope {
        Some(name) => vec![snap.keyspace(name)?],
        None => snap
            .keyspaces
            .values()
            .filter(|k| !k.is_system() && !k.is_virtual)
            .collect(),
    };
    let restricted = !only_tables.is_empty();
    let mut items = Vec::new();
    let item = |path: ObjectPath,
                kind: ObjectKind,
                section: DdlSection,
                sql: String,
                depends_on: Vec<ObjectPath>| DdlItem {
        path,
        kind,
        section,
        sql,
        language: DdlLanguage::Cql,
        depends_on,
    };
    for k in keyspaces {
        if k.is_virtual {
            return Err(Error::Unsupported("DDL of a virtual keyspace".into()));
        }
        let q = opts.qualify_schema;
        let ks = k.name.as_str();
        let selected: Vec<&TableInfo> = k
            .tables
            .values()
            .filter(|t| !restricted || only_tables.iter().any(|(kk, tn)| kk == ks && tn == &t.name))
            .collect();
        if restricted && selected.is_empty() {
            continue;
        }
        let ks_dep = ks_path(ks);
        if !restricted {
            if opts.include_drop {
                items.push(item(
                    ks_dep.clone(),
                    ObjectKind::Database,
                    DdlSection::PreData,
                    format!("DROP KEYSPACE IF EXISTS {};", quote_ident(ks)),
                    vec![],
                ));
            }
            items.push(item(
                ks_dep.clone(),
                ObjectKind::Database,
                DdlSection::PreData,
                keyspace_ddl(k, opts.if_not_exists),
                vec![],
            ));
        }
        let wanted_types = restricted.then(|| types_used_by(k, &selected));
        for u in ordered_types(k, wanted_types.as_ref()) {
            let mut deps = Vec::new();
            for (_, t) in &u.fields {
                t.udt_names(&mut deps);
            }
            if opts.include_drop {
                items.push(item(
                    type_path(ks, &u.name),
                    ObjectKind::Type,
                    DdlSection::PreData,
                    format!("DROP TYPE IF EXISTS {};", name_of(ks, &u.name, q)),
                    vec![],
                ));
            }
            items.push(item(
                type_path(ks, &u.name),
                ObjectKind::Type,
                DdlSection::PreData,
                udt_ddl(ks, u, q, opts.if_not_exists),
                deps.iter().map(|d| type_path(ks, d)).collect(),
            ));
        }
        for t in &selected {
            let mut deps = Vec::new();
            for c in &t.columns {
                c.ty.udt_names(&mut deps);
            }
            if opts.include_drop {
                items.push(item(
                    table_path(ks, &t.name, false),
                    ObjectKind::Table,
                    DdlSection::PreData,
                    format!("DROP TABLE IF EXISTS {};", name_of(ks, &t.name, q)),
                    vec![],
                ));
            }
            items.push(item(
                table_path(ks, &t.name, false),
                ObjectKind::Table,
                DdlSection::PreData,
                table_ddl(ks, t, q, opts.if_not_exists),
                deps.iter().map(|d| type_path(ks, d)).collect(),
            ));
        }
        if !restricted && opts.include_routines {
            for f in &k.functions {
                items.push(item(
                    function_path(ks, &f.signature()),
                    ObjectKind::Function,
                    DdlSection::PostData,
                    function_ddl(ks, f, q, opts.if_not_exists),
                    vec![],
                ));
            }
            for a in &k.aggregates {
                items.push(item(
                    aggregate_path(ks, &a.signature()),
                    ObjectKind::Function,
                    DdlSection::PostData,
                    aggregate_ddl(ks, a, q, opts.if_not_exists),
                    vec![],
                ));
            }
        }
        if opts.include_indexes {
            for t in &selected {
                for i in &t.indexes {
                    items.push(item(
                        index_path(ks, t, &i.name),
                        ObjectKind::Index,
                        DdlSection::PostData,
                        index_ddl(ks, &t.name, i, q, opts.if_not_exists),
                        vec![table_path(ks, &t.name, false)],
                    ));
                }
            }
        }
        if opts.include_triggers {
            for t in &selected {
                for (name, class) in &t.triggers {
                    items.push(item(
                        trigger_path(ks, name),
                        ObjectKind::Trigger,
                        DdlSection::PostData,
                        trigger_ddl(ks, &t.name, name, class, q),
                        vec![table_path(ks, &t.name, false)],
                    ));
                }
            }
        }
        if !restricted && opts.include_views {
            for v in k.views.values() {
                let base = v.base_table.clone().unwrap_or_default();
                if opts.include_drop {
                    items.push(item(
                        table_path(ks, &v.name, true),
                        ObjectKind::MaterializedView,
                        DdlSection::PostData,
                        format!(
                            "DROP MATERIALIZED VIEW IF EXISTS {};",
                            name_of(ks, &v.name, q)
                        ),
                        vec![],
                    ));
                }
                items.push(item(
                    table_path(ks, &v.name, true),
                    ObjectKind::MaterializedView,
                    DdlSection::PostData,
                    view_ddl(ks, v, q, opts.if_not_exists),
                    vec![table_path(ks, &base, false)],
                ));
                if opts.include_indexes {
                    for i in &v.indexes {
                        items.push(item(
                            index_path(ks, v, &i.name),
                            ObjectKind::Index,
                            DdlSection::PostData,
                            index_ddl(ks, &v.name, i, q, opts.if_not_exists),
                            vec![table_path(ks, &v.name, true)],
                        ));
                    }
                }
            }
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ColumnInfo;
    use crate::types::CqlType;
    use std::collections::BTreeMap;

    fn col(name: &str, kind: ColKind, pos: i64, ty: &str, desc: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            kind,
            position: pos,
            descending: desc,
            ty: CqlType::parse(ty),
        }
    }

    fn sample() -> Snapshot {
        let mut ks = KeyspaceInfo {
            name: "shop".into(),
            durable_writes: true,
            replication: BTreeMap::from([
                ("class".to_string(), "SimpleStrategy".to_string()),
                ("replication_factor".to_string(), "1".to_string()),
            ]),
            tablets: Some(false),
            ..KeyspaceInfo::default()
        };
        ks.types.insert(
            "address".into(),
            UdtInfo {
                name: "address".into(),
                fields: vec![("zip".into(), CqlType::Int)],
            },
        );
        ks.types.insert(
            "person".into(),
            UdtInfo {
                name: "person".into(),
                fields: vec![("home".into(), CqlType::parse("frozen<address>"))],
            },
        );
        ks.tables.insert(
            "orders".into(),
            TableInfo {
                name: "orders".into(),
                columns: vec![
                    col("customer", ColKind::Partition, 0, "text", false),
                    col("day", ColKind::Partition, 1, "date", false),
                    col("ts", ColKind::Clustering, 0, "timestamp", true),
                    col("who", ColKind::Regular, -1, "frozen<person>", false),
                    col("note", ColKind::Static, -1, "text", false),
                ],
                options: vec![("comment".into(), "'x'".into())],
                indexes: vec![IndexInfo {
                    name: "note_idx".into(),
                    kind: "COMPOSITES".into(),
                    target: "note".into(),
                    class: None,
                    options: BTreeMap::new(),
                }],
                ..TableInfo::default()
            },
        );
        let mut snap = Snapshot::default();
        snap.keyspaces.insert("shop".into(), ks);
        snap
    }

    #[test]
    fn table_statement() {
        let snap = sample();
        let t = &snap.keyspaces["shop"].tables["orders"];
        let ddl = table_ddl("shop", t, true, false);
        assert_eq!(
            ddl,
            "CREATE TABLE shop.orders (\n    customer text,\n    day date,\n    ts timestamp,\n    who frozen<person>,\n    note text STATIC,\n    PRIMARY KEY ((customer, day), ts)\n)\n    WITH CLUSTERING ORDER BY (ts DESC)\n    AND comment = 'x';"
        );
        let single = table_ddl("shop", t, false, true);
        assert!(single.starts_with("CREATE TABLE IF NOT EXISTS orders ("));
    }

    #[test]
    fn keyspace_and_types_in_order() {
        let snap = sample();
        let items = generate(&snap, Some("shop"), &[], &DdlOptions::default()).unwrap();
        let sql: Vec<&str> = items.iter().map(|i| i.sql.as_str()).collect();
        assert!(sql[0].starts_with("CREATE KEYSPACE shop WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'} AND durable_writes = true AND tablets = {'enabled': false}"));
        let addr = sql
            .iter()
            .position(|s| s.starts_with("CREATE TYPE shop.address"))
            .unwrap();
        let person = sql
            .iter()
            .position(|s| s.starts_with("CREATE TYPE shop.person"))
            .unwrap();
        let table = sql
            .iter()
            .position(|s| s.starts_with("CREATE TABLE"))
            .unwrap();
        let idx = sql
            .iter()
            .position(|s| s.starts_with("CREATE INDEX"))
            .unwrap();
        assert!(addr < person && person < table && table < idx);
        assert_eq!(items[idx].section, DdlSection::PostData);
        assert_eq!(items[table].section, DdlSection::PreData);
        assert!(items.iter().all(|i| i.language == DdlLanguage::Cql));
    }

    #[test]
    fn restricted_to_tables_keeps_needed_types() {
        let snap = sample();
        let items = generate(
            &snap,
            Some("shop"),
            &[("shop".into(), "orders".into())],
            &DdlOptions::default(),
        )
        .unwrap();
        assert!(!items.iter().any(|i| i.kind == ObjectKind::Database));
        assert_eq!(
            items.iter().filter(|i| i.kind == ObjectKind::Type).count(),
            2
        );
        assert!(items.iter().any(|i| i.kind == ObjectKind::Index));
    }

    #[test]
    fn drop_and_if_not_exists_options() {
        let snap = sample();
        let opts = DdlOptions {
            include_drop: true,
            if_not_exists: true,
            qualify_schema: false,
            ..DdlOptions::default()
        };
        let items = generate(&snap, Some("shop"), &[], &opts).unwrap();
        assert!(items[0].sql.starts_with("DROP KEYSPACE IF EXISTS shop"));
        assert!(
            items
                .iter()
                .any(|i| i.sql.starts_with("DROP TABLE IF EXISTS orders;"))
        );
        assert!(
            items
                .iter()
                .any(|i| i.sql.starts_with("CREATE TYPE IF NOT EXISTS address"))
        );
    }

    #[test]
    fn routines_views_and_indexes() {
        let f = FunctionInfo {
            name: "plus".into(),
            arg_names: vec!["a".into(), "b".into()],
            arg_types: vec!["int".into(), "int".into()],
            return_type: "int".into(),
            language: "java".into(),
            body: "return a + b;".into(),
            called_on_null: false,
        };
        assert_eq!(
            function_ddl("ks", &f, true, false),
            "CREATE FUNCTION ks.plus (a int, b int)\n    RETURNS NULL ON NULL INPUT\n    RETURNS int\n    LANGUAGE java\n    AS $$return a + b;$$;"
        );
        assert_eq!(f.signature(), "plus(int, int)");
        let a = AggregateInfo {
            name: "total".into(),
            arg_types: vec!["int".into()],
            state_func: "plus".into(),
            state_type: "int".into(),
            final_func: None,
            initcond: Some("0".into()),
            return_type: "int".into(),
        };
        assert_eq!(
            aggregate_ddl("ks", &a, true, true),
            "CREATE AGGREGATE IF NOT EXISTS ks.total (int)\n    SFUNC plus\n    STYPE int\n    INITCOND 0;"
        );
        let idx = IndexInfo {
            name: "i".into(),
            kind: "CUSTOM".into(),
            target: "values(Tags)".into(),
            class: Some("org.X".into()),
            options: BTreeMap::from([
                ("target".into(), "values(Tags)".into()),
                ("class_name".into(), "org.X".into()),
                ("mode".into(), "CONTAINS".into()),
            ]),
        };
        assert_eq!(
            index_ddl("ks", "t", &idx, true, false),
            "CREATE CUSTOM INDEX i ON ks.t (values(\"Tags\")) USING 'org.X' WITH OPTIONS = {'mode': 'CONTAINS'};"
        );
        let view = TableInfo {
            name: "by_day".into(),
            is_view: true,
            base_table: Some("orders".into()),
            where_clause: Some("day IS NOT NULL".into()),
            include_all_columns: true,
            columns: vec![col("day", ColKind::Partition, 0, "date", false)],
            ..TableInfo::default()
        };
        assert_eq!(
            view_ddl("ks", &view, true, false),
            "CREATE MATERIALIZED VIEW ks.by_day AS\n    SELECT *\n    FROM ks.orders\n    WHERE day IS NOT NULL\n    PRIMARY KEY (day);"
        );
        assert_eq!(dollar_quote("a$$b"), "'a$$b'");
        assert_eq!(
            trigger_ddl("ks", "t", "tr", "org.T", true),
            "CREATE TRIGGER tr ON ks.t USING 'org.T';"
        );
    }

    #[test]
    fn unknown_keyspace_is_not_found() {
        let snap = sample();
        let err = generate(&snap, Some("nope"), &[], &DdlOptions::default()).unwrap_err();
        assert_eq!(err.kind(), "notFound");
    }
}
