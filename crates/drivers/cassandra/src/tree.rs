//! Object tree, table details, search and the schema model.

use std::sync::Arc;

use dbstudio_core::{
    ColumnDetail, Error, IndexDetail, ModelDetail, ObjectKind, ObjectNode, ObjectPath,
    ObjectSearchRequest, Result, SchemaModel, SchemaModelRequest, TableDetails, TableModel, folder,
};

use crate::core::Core;
use crate::ddl::{
    aggregate_ddl, aggregate_path, function_ddl, function_path, index_ddl, table_ddl, table_path,
    trigger_ddl, trigger_path, type_path, udt_ddl, view_ddl,
};
use crate::schema::{ColKind, ColumnInfo, KeyspaceInfo, Snapshot, TableInfo};

/// A node of the tree located from an [`ObjectPath`] (Folder segments are optional).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Root,
    SystemFolder,
    Keyspace(String),
    KsFolder(String, String),
    Table(String, String),
    View(String, String),
    RelFolder(String, String, String),
    Column(String, String, String),
    Index(String, String, String),
    Type(String, String),
    Function(String, String),
    Aggregate(String, String),
    Trigger(String, String),
}

impl Node {
    /// Keyspace, table and view name of a relation node.
    pub(crate) fn relation(&self) -> Option<(&str, &str, bool)> {
        match self {
            Node::Table(k, t) => Some((k, t, false)),
            Node::View(k, t) => Some((k, t, true)),
            _ => None,
        }
    }
}

fn bad(path: &ObjectPath) -> Error {
    Error::Invalid(format!("not a Cassandra object path: {path:?}"))
}

/// Locate the node of `path`.
pub(crate) fn parse_path(path: &ObjectPath) -> Result<Node> {
    let mut segs = path.segments.as_slice();
    if segs.is_empty() {
        return Ok(Node::Root);
    }
    if segs[0].kind == ObjectKind::Folder {
        if segs.len() == 1 {
            return Ok(Node::SystemFolder);
        }
        segs = &segs[1..];
    }
    if segs[0].kind != ObjectKind::Database {
        return Err(bad(path));
    }
    let ks = segs[0].name.clone();
    let mut rest = &segs[1..];
    let mut ks_folder: Option<String> = None;
    if let Some(first) = rest.first() {
        if first.kind == ObjectKind::Folder {
            if rest.len() == 1 {
                return Ok(Node::KsFolder(ks, first.name.clone()));
            }
            ks_folder = Some(first.name.clone());
            rest = &rest[1..];
        }
    }
    let Some(obj) = rest.first() else {
        return Ok(Node::Keyspace(ks));
    };
    let name = obj.name.clone();
    let node = match obj.kind {
        ObjectKind::Table | ObjectKind::Collection => Node::Table(ks.clone(), name.clone()),
        ObjectKind::View | ObjectKind::MaterializedView => Node::View(ks.clone(), name.clone()),
        ObjectKind::Type => return Ok(Node::Type(ks, name)),
        ObjectKind::Function => {
            return Ok(if ks_folder.as_deref() == Some(folder::AGGREGATES) {
                Node::Aggregate(ks, name)
            } else {
                Node::Function(ks, name)
            });
        }
        ObjectKind::Trigger => return Ok(Node::Trigger(ks, name)),
        _ => return Err(bad(path)),
    };
    let rest = &rest[1..];
    let Some(sub) = rest.first() else {
        return Ok(node);
    };
    let (is_view, tname) = match &node {
        Node::View(_, t) => (true, t.clone()),
        Node::Table(_, t) => (false, t.clone()),
        _ => unreachable!(),
    };
    let _ = is_view;
    match sub.kind {
        ObjectKind::Folder => {
            if rest.len() == 1 {
                return Ok(Node::RelFolder(ks, tname, sub.name.clone()));
            }
            let leaf = &rest[1];
            match leaf.kind {
                ObjectKind::Column => Ok(Node::Column(ks, tname, leaf.name.clone())),
                ObjectKind::Index => Ok(Node::Index(ks, tname, leaf.name.clone())),
                _ => Err(bad(path)),
            }
        }
        ObjectKind::Column => Ok(Node::Column(ks, tname, sub.name.clone())),
        ObjectKind::Index => Ok(Node::Index(ks, tname, sub.name.clone())),
        _ => Err(bad(path)),
    }
}

fn node(
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

fn folder_node(parent: &ObjectPath, name: &str) -> ObjectNode {
    node(
        parent.child(ObjectKind::Folder, name),
        ObjectKind::Folder,
        name,
        true,
        None,
    )
}

fn db_node(parent: &ObjectPath, k: &KeyspaceInfo) -> ObjectNode {
    let has = !k.tables.is_empty()
        || !k.views.is_empty()
        || !k.types.is_empty()
        || !k.functions.is_empty()
        || !k.aggregates.is_empty();
    node(
        parent.child(ObjectKind::Database, &k.name),
        ObjectKind::Database,
        &k.name,
        has,
        k.replication
            .get("class")
            .map(|c| c.rsplit('.').next().unwrap_or(c).to_string()),
    )
}

fn column_marker(c: &ColumnInfo) -> &'static str {
    match c.kind {
        ColKind::Partition => "partition key",
        ColKind::Clustering => "clustering key",
        ColKind::Static => "static",
        ColKind::Regular => "",
    }
}

fn column_detail_text(c: &ColumnInfo) -> String {
    match column_marker(c) {
        "" => c.ty.to_string(),
        m => format!("{} · {m}", c.ty),
    }
}

/// Column name inside an index target (`values(tags)` gives `tags`).
fn target_column(target: &str) -> String {
    for wrap in ["keys", "values", "entries", "full"] {
        if let Some(inner) = target
            .strip_prefix(wrap)
            .and_then(|r| r.strip_prefix('('))
            .and_then(|r| r.strip_suffix(')'))
        {
            return inner.trim_matches('"').to_string();
        }
    }
    target.trim_matches('"').to_string()
}

fn column_detail(c: &ColumnInfo, ordinal: u32) -> ColumnDetail {
    ColumnDetail {
        name: c.name.clone(),
        ordinal,
        type_name: c.ty.to_string(),
        value_kind: c.ty.value_kind(),
        nullable: !c.is_key(),
        default: None,
        comment: None,
        primary_key: c.is_key(),
        extra: match c.kind {
            ColKind::Partition => Some("partition key".into()),
            ColKind::Clustering => Some(format!(
                "clustering key {}",
                if c.descending { "desc" } else { "asc" }
            )),
            ColKind::Static => Some("static".into()),
            ColKind::Regular => None,
        },
    }
}

/// Sort: exact match, prefix match, substring match; each group by name then path.
fn rank(name: &str, query: &str) -> u8 {
    let n = name.to_lowercase();
    if n == query {
        0
    } else if n.starts_with(query) {
        1
    } else {
        2
    }
}

impl Core {
    pub(crate) async fn snapshot(&self) -> Result<Arc<Snapshot>> {
        self.cache.get(&self.pool.base()).await
    }

    /// Keyspaces to list at the root and under the `system` folder.
    fn user_keyspaces(snap: &Snapshot) -> Vec<&KeyspaceInfo> {
        snap.keyspaces.values().filter(|k| !k.is_system()).collect()
    }

    pub(crate) async fn children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
        let snap = self.snapshot().await?;
        let target = parse_path(parent)?;
        let mut out = Vec::new();
        match target {
            Node::Root => {
                for k in Self::user_keyspaces(&snap) {
                    out.push(db_node(parent, k));
                }
                if self.pool.settings.show_system && snap.keyspaces.values().any(|k| k.is_system())
                {
                    out.push(node(
                        parent.child(ObjectKind::Folder, "system"),
                        ObjectKind::Folder,
                        "system",
                        true,
                        None,
                    ));
                }
            }
            Node::SystemFolder => {
                for k in snap.keyspaces.values().filter(|k| k.is_system()) {
                    out.push(db_node(parent, k));
                }
            }
            Node::Keyspace(ks) => {
                let k = snap.keyspace(&ks)?;
                if !k.tables.is_empty() {
                    out.push(folder_node(parent, folder::TABLES));
                }
                if !k.views.is_empty() {
                    out.push(folder_node(parent, folder::MATERIALIZED_VIEWS));
                }
                if !k.types.is_empty() {
                    out.push(folder_node(parent, folder::TYPES));
                }
                if !k.functions.is_empty() {
                    out.push(folder_node(parent, folder::FUNCTIONS));
                }
                if !k.aggregates.is_empty() {
                    out.push(folder_node(parent, folder::AGGREGATES));
                }
                if !self.facts.scylla && k.tables.values().any(|t| !t.triggers.is_empty()) {
                    out.push(folder_node(parent, folder::TRIGGERS));
                }
            }
            Node::KsFolder(ks, f) => {
                let k = snap.keyspace(&ks)?;
                match f.as_str() {
                    folder::TABLES => {
                        for t in k.tables.values() {
                            out.push(node(
                                parent.child(ObjectKind::Table, &t.name),
                                ObjectKind::Table,
                                &t.name,
                                true,
                                t.is_virtual.then(|| "virtual".to_string()),
                            ));
                        }
                    }
                    folder::MATERIALIZED_VIEWS => {
                        for v in k.views.values() {
                            out.push(node(
                                parent.child(ObjectKind::MaterializedView, &v.name),
                                ObjectKind::MaterializedView,
                                &v.name,
                                true,
                                v.base_table.as_ref().map(|b| format!("on {b}")),
                            ));
                        }
                    }
                    folder::TYPES => {
                        for u in k.types.values() {
                            out.push(node(
                                parent.child(ObjectKind::Type, &u.name),
                                ObjectKind::Type,
                                &u.name,
                                false,
                                Some(format!("{} fields", u.fields.len())),
                            ));
                        }
                    }
                    folder::FUNCTIONS => {
                        for f in &k.functions {
                            let sig = f.signature();
                            out.push(node(
                                parent.child(ObjectKind::Function, &sig),
                                ObjectKind::Function,
                                &sig,
                                false,
                                Some(f.return_type.clone()),
                            ));
                        }
                    }
                    folder::AGGREGATES => {
                        for a in &k.aggregates {
                            let sig = a.signature();
                            out.push(node(
                                parent.child(ObjectKind::Function, &sig),
                                ObjectKind::Function,
                                &sig,
                                false,
                                Some(a.return_type.clone()),
                            ));
                        }
                    }
                    folder::TRIGGERS => {
                        for t in k.tables.values() {
                            for (name, _) in &t.triggers {
                                out.push(node(
                                    parent.child(ObjectKind::Trigger, name),
                                    ObjectKind::Trigger,
                                    name,
                                    false,
                                    Some(format!("on {}", t.name)),
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Node::Table(ks, name) | Node::View(ks, name) => {
                let t = snap.relation(&ks, &name)?;
                out.push(folder_node(parent, folder::COLUMNS));
                if !t.indexes.is_empty() {
                    out.push(folder_node(parent, folder::INDEXES));
                }
            }
            Node::RelFolder(ks, name, f) => {
                let t = snap.relation(&ks, &name)?;
                match f.as_str() {
                    folder::COLUMNS => {
                        for c in &t.columns {
                            out.push(node(
                                parent.child(ObjectKind::Column, &c.name),
                                ObjectKind::Column,
                                &c.name,
                                false,
                                Some(column_detail_text(c)),
                            ));
                        }
                    }
                    folder::INDEXES => {
                        for i in &t.indexes {
                            out.push(node(
                                parent.child(ObjectKind::Index, &i.name),
                                ObjectKind::Index,
                                &i.name,
                                false,
                                Some(i.target.clone()),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Node::Column(..)
            | Node::Index(..)
            | Node::Type(..)
            | Node::Function(..)
            | Node::Aggregate(..)
            | Node::Trigger(..) => {}
        }
        Ok(out)
    }

    pub(crate) async fn details(&self, path: &ObjectPath) -> Result<TableDetails> {
        let snap = self.snapshot().await?;
        let target = parse_path(path)?;
        let Some((ks, name, view)) = target.relation() else {
            return Err(Error::Invalid("table_details needs a table or view".into()));
        };
        let t = snap.relation(ks, name)?;
        let mut indexes = Vec::new();
        indexes.push(IndexDetail {
            name: "PRIMARY".into(),
            columns: t.key_columns().iter().map(|c| c.name.clone()).collect(),
            unique: true,
            primary: true,
            method: Some("primary key".into()),
            definition: None,
            partial: false,
        });
        for i in &t.indexes {
            indexes.push(IndexDetail {
                name: i.name.clone(),
                columns: vec![target_column(&i.target)],
                unique: false,
                primary: false,
                method: Some(i.kind.to_lowercase()),
                definition: Some(index_ddl(ks, name, i, true, false)),
                partial: false,
            });
        }
        let (estimated_rows, size_bytes) = if t.is_virtual {
            (None, None)
        } else {
            self.estimates(ks, name).await
        };
        Ok(TableDetails {
            path: table_path(ks, name, view),
            kind: if view {
                ObjectKind::MaterializedView
            } else {
                ObjectKind::Table
            },
            comment: t.comment.clone(),
            columns: t
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| column_detail(c, i as u32 + 1))
                .collect(),
            indexes,
            foreign_keys: Vec::new(),
            estimated_rows,
            size_bytes,
        })
    }

    /// Row count and size from `system.size_estimates` (best effort, per node).
    async fn estimates(&self, ks: &str, table: &str) -> (Option<u64>, Option<u64>) {
        let cql = format!(
            "SELECT partitions_count, mean_partition_size FROM system.size_estimates \
             WHERE keyspace_name = {} AND table_name = {}",
            crate::ident::quote_text(ks),
            crate::ident::quote_text(table)
        );
        let Ok(rows) = self.rows(&cql).await else {
            return (None, None);
        };
        let mut count: i128 = 0;
        let mut size: i128 = 0;
        for r in &rows {
            let c = i128::from(r.int("partitions_count").unwrap_or(0).max(0));
            let m = i128::from(r.int("mean_partition_size").unwrap_or(0).max(0));
            count += c;
            size += c * m;
        }
        if count == 0 {
            (None, None)
        } else {
            (
                u64::try_from(count).ok(),
                u64::try_from(size).ok().filter(|s| *s > 0),
            )
        }
    }

    pub(crate) async fn search(&self, req: ObjectSearchRequest) -> Result<Vec<ObjectNode>> {
        let query = req.query.trim().to_lowercase();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = req.limit.clamp(1, 1000) as usize;
        let snap = self.snapshot().await?;
        let scope_ks = match parse_path(&req.scope)? {
            Node::Root | Node::SystemFolder => None,
            Node::Keyspace(k) | Node::KsFolder(k, _) => Some(k),
            _ => {
                return Err(Error::Invalid(
                    "search scope must be a keyspace or the root".into(),
                ));
            }
        };
        let wants = |k: ObjectKind| {
            if req.kinds.is_empty() {
                matches!(
                    k,
                    ObjectKind::Table
                        | ObjectKind::View
                        | ObjectKind::MaterializedView
                        | ObjectKind::Function
                        | ObjectKind::Procedure
                        | ObjectKind::Sequence
                )
            } else {
                req.kinds.contains(&k)
            }
        };
        let mut found: Vec<ObjectNode> = Vec::new();
        for k in snap.keyspaces.values() {
            if let Some(s) = &scope_ks {
                if &k.name != s {
                    continue;
                }
            } else if k.is_system() && !self.pool.settings.show_system {
                continue;
            }
            let detail = Some(k.name.clone());
            let mut push = |path: ObjectPath, kind: ObjectKind, name: &str| {
                if name.to_lowercase().contains(&query) {
                    found.push(node(
                        path,
                        kind,
                        name,
                        kind == ObjectKind::Table || kind == ObjectKind::MaterializedView,
                        detail.clone(),
                    ));
                }
            };
            if wants(ObjectKind::Table) {
                for t in k.tables.values() {
                    push(
                        table_path(&k.name, &t.name, false),
                        ObjectKind::Table,
                        &t.name,
                    );
                }
            }
            if wants(ObjectKind::MaterializedView) || wants(ObjectKind::View) {
                for v in k.views.values() {
                    push(
                        table_path(&k.name, &v.name, true),
                        ObjectKind::MaterializedView,
                        &v.name,
                    );
                }
            }
            if wants(ObjectKind::Function) {
                for f in &k.functions {
                    push(
                        function_path(&k.name, &f.signature()),
                        ObjectKind::Function,
                        &f.signature(),
                    );
                }
                for a in &k.aggregates {
                    push(
                        aggregate_path(&k.name, &a.signature()),
                        ObjectKind::Function,
                        &a.signature(),
                    );
                }
            }
            if wants(ObjectKind::Type) {
                for u in k.types.values() {
                    push(type_path(&k.name, &u.name), ObjectKind::Type, &u.name);
                }
            }
            if wants(ObjectKind::Trigger) {
                for t in k.tables.values() {
                    for (name, _) in &t.triggers {
                        push(trigger_path(&k.name, name), ObjectKind::Trigger, name);
                    }
                }
            }
        }
        found.sort_by(|a, b| {
            rank(&a.name, &query)
                .cmp(&rank(&b.name, &query))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| format!("{:?}", a.path).cmp(&format!("{:?}", b.path)))
        });
        found.truncate(limit);
        Ok(found)
    }

    pub(crate) async fn model(&self, req: SchemaModelRequest) -> Result<SchemaModel> {
        let snap = self.snapshot().await?;
        let scope_ks = match parse_path(&req.scope)? {
            Node::Root | Node::SystemFolder => None,
            Node::Keyspace(k) | Node::KsFolder(k, _) => Some(k),
            _ => {
                return Err(Error::Invalid(
                    "schema model scope must be a keyspace".into(),
                ));
            }
        };
        let wanted: Vec<(String, String)> = req
            .tables
            .iter()
            .filter_map(|p| parse_path(p).ok())
            .filter_map(|n| n.relation().map(|(k, t, _)| (k.to_string(), t.to_string())))
            .collect();
        let mut tables: Vec<TableModel> = Vec::new();
        for k in snap.keyspaces.values() {
            if let Some(s) = &scope_ks {
                if &k.name != s {
                    continue;
                }
            } else if k.is_system() {
                continue;
            }
            let mut rels: Vec<&TableInfo> = k.tables.values().collect();
            if req.include_views {
                rels.extend(k.views.values());
            }
            for t in rels {
                if !wanted.is_empty()
                    && !wanted.iter().any(|(kk, tn)| kk == &k.name && tn == &t.name)
                {
                    continue;
                }
                let columns: Vec<ColumnDetail> = t
                    .columns
                    .iter()
                    .filter(|c| req.detail == ModelDetail::Full || c.is_key())
                    .enumerate()
                    .map(|(i, c)| column_detail(c, i as u32 + 1))
                    .collect();
                tables.push(TableModel {
                    path: table_path(&k.name, &t.name, t.is_view),
                    kind: if t.is_view {
                        ObjectKind::MaterializedView
                    } else {
                        ObjectKind::Table
                    },
                    schema: None,
                    name: t.name.clone(),
                    comment: if req.include_comments {
                        t.comment.clone()
                    } else {
                        None
                    },
                    columns,
                    primary_key: t.key_columns().iter().map(|c| c.name.clone()).collect(),
                    unique_keys: Vec::new(),
                    estimated_rows: None,
                });
            }
        }
        tables.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| format!("{:?}", a.path).cmp(&format!("{:?}", b.path)))
        });
        let truncated = tables.len() > 2000;
        tables.truncate(2000);
        Ok(SchemaModel {
            scope: req.scope,
            tables,
            relations: Vec::new(),
            inferred: false,
            truncated,
        })
    }

    pub(crate) async fn ddl_of(&self, path: &ObjectPath) -> Result<String> {
        let snap = self.snapshot().await?;
        let target = parse_path(path)?;
        let none = |what: &str| Error::NotFound(what.to_string());
        let text = match target {
            Node::Keyspace(ks) => {
                let items = crate::ddl::generate(
                    &snap,
                    Some(&ks),
                    &[],
                    &dbstudio_core::DdlOptions::default(),
                )?;
                items
                    .iter()
                    .map(|i| i.sql.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
            Node::Table(ks, name) | Node::View(ks, name) => {
                let k = snap.keyspace(&ks)?;
                let t = k
                    .relation(&name)
                    .ok_or_else(|| none(&format!("{ks}.{name}")))?;
                if t.is_view {
                    view_ddl(&ks, t, true, false)
                } else {
                    let mut parts = vec![table_ddl(&ks, t, true, false)];
                    for i in &t.indexes {
                        parts.push(index_ddl(&ks, &name, i, true, false));
                    }
                    for (n, c) in &t.triggers {
                        parts.push(trigger_ddl(&ks, &name, n, c, true));
                    }
                    parts.join("\n\n")
                }
            }
            Node::Index(ks, table, idx) => {
                let t = snap.relation(&ks, &table)?;
                let i = t
                    .indexes
                    .iter()
                    .find(|i| i.name == idx)
                    .ok_or_else(|| none(&format!("index {idx}")))?;
                index_ddl(&ks, &table, i, true, false)
            }
            Node::Type(ks, name) => {
                let k = snap.keyspace(&ks)?;
                let u = k
                    .types
                    .get(&name)
                    .ok_or_else(|| none(&format!("type {name}")))?;
                udt_ddl(&ks, u, true, false)
            }
            Node::Function(ks, sig) => {
                let k = snap.keyspace(&ks)?;
                let f = k
                    .functions
                    .iter()
                    .find(|f| f.signature() == sig || f.name == sig)
                    .ok_or_else(|| none(&format!("function {sig}")))?;
                function_ddl(&ks, f, true, false)
            }
            Node::Aggregate(ks, sig) => {
                let k = snap.keyspace(&ks)?;
                let a = k
                    .aggregates
                    .iter()
                    .find(|a| a.signature() == sig || a.name == sig)
                    .ok_or_else(|| none(&format!("aggregate {sig}")))?;
                aggregate_ddl(&ks, a, true, false)
            }
            Node::Trigger(ks, name) => {
                let k = snap.keyspace(&ks)?;
                let (t, class) = k
                    .tables
                    .values()
                    .find_map(|t| {
                        t.triggers
                            .iter()
                            .find(|(n, _)| n == &name)
                            .map(|(_, c)| (t, c.clone()))
                    })
                    .ok_or_else(|| none(&format!("trigger {name}")))?;
                trigger_ddl(&ks, &t.name, &name, &class, true)
            }
            _ => return Err(Error::Unsupported("object_ddl".into())),
        };
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(items: &[(ObjectKind, &str)]) -> ObjectPath {
        let mut path = ObjectPath::root();
        for (k, n) in items {
            path = path.child(*k, *n);
        }
        path
    }

    #[test]
    fn paths_with_and_without_folders() {
        use ObjectKind::*;
        assert_eq!(parse_path(&ObjectPath::root()).unwrap(), Node::Root);
        assert_eq!(
            parse_path(&p(&[(Folder, "system")])).unwrap(),
            Node::SystemFolder
        );
        assert_eq!(
            parse_path(&p(&[(Folder, "system"), (Database, "system_schema")])).unwrap(),
            Node::Keyspace("system_schema".into())
        );
        assert_eq!(
            parse_path(&p(&[(Database, "ks"), (Folder, "tables"), (Table, "t")])).unwrap(),
            Node::Table("ks".into(), "t".into())
        );
        assert_eq!(
            parse_path(&p(&[(Database, "ks"), (Table, "t")])).unwrap(),
            Node::Table("ks".into(), "t".into())
        );
        assert_eq!(
            parse_path(&p(&[
                (Database, "ks"),
                (Folder, "tables"),
                (Table, "t"),
                (Folder, "columns"),
                (Column, "c")
            ]))
            .unwrap(),
            Node::Column("ks".into(), "t".into(), "c".into())
        );
        assert_eq!(
            parse_path(&p(&[
                (Database, "ks"),
                (Folder, "materializedViews"),
                (MaterializedView, "v")
            ]))
            .unwrap(),
            Node::View("ks".into(), "v".into())
        );
        assert_eq!(
            parse_path(&p(&[
                (Database, "ks"),
                (Folder, "aggregates"),
                (Function, "a(int)")
            ]))
            .unwrap(),
            Node::Aggregate("ks".into(), "a(int)".into())
        );
        assert_eq!(
            parse_path(&p(&[
                (Database, "ks"),
                (Folder, "functions"),
                (Function, "f(int)")
            ]))
            .unwrap(),
            Node::Function("ks".into(), "f(int)".into())
        );
        assert_eq!(
            parse_path(&p(&[(Database, "ks"), (Folder, "tables")])).unwrap(),
            Node::KsFolder("ks".into(), "tables".into())
        );
        assert!(parse_path(&p(&[(Table, "t")])).is_err());
    }

    #[test]
    fn index_target_columns() {
        assert_eq!(target_column("values(tags)"), "tags");
        assert_eq!(target_column("name"), "name");
        assert_eq!(target_column("\"Name\""), "Name");
    }

    #[test]
    fn ranking() {
        assert_eq!(rank("User", "user"), 0);
        assert_eq!(rank("users", "user"), 1);
        assert_eq!(rank("all_users", "user"), 2);
    }
}
