//! Default implementations of the new `Session` methods built only from the phase 1 methods
//! (`list_children`, `table_details`), so existing drivers get the features immediately (slower
//! than a dedicated catalog query).
//!
//! The helpers are generic (`S: Session + ?Sized`) so that default trait methods can call them
//! with `self`.

use std::collections::BTreeSet;

use crate::model::ModelDetail;
use crate::{
    ChildrenPage, ChildrenPageRequest, ColumnDetail, Editability, Error, NotEditableReason,
    ObjectKind, ObjectNode, ObjectPath, Relation, Result, SchemaModel, SchemaModelRequest, Session,
    TableDetails, TableModel,
};

/// Largest number of tables [`schema_model_via_details`] processes (one `table_details` call
/// each); more => `Unsupported("schema_model")`.
pub const MAX_FALLBACK_TABLES: usize = 300;

/// Case-insensitive glob match supporting `*`, `?`, `[...]` (ranges, `!`/`^` negation) and `\`
/// escapes.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    glob(&p, &t)
}

fn glob(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        let advanced = match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
                continue;
            }
            Some('?') => {
                pi += 1;
                true
            }
            Some('[') => match match_class(&p[pi..], t[ti]) {
                Some((true, used)) => {
                    pi += used;
                    true
                }
                Some((false, _)) => false,
                // Unterminated class: literal `[`.
                None => {
                    if t[ti] == '[' {
                        pi += 1;
                        true
                    } else {
                        false
                    }
                }
            },
            Some('\\') if pi + 1 < p.len() => {
                if p[pi + 1] == t[ti] {
                    pi += 2;
                    true
                } else {
                    false
                }
            }
            Some(c) if *c == t[ti] => {
                pi += 1;
                true
            }
            Some(_) => false,
            None => false,
        };
        if advanced {
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// Match `c` against the class starting at `p[0] == '['`. Returns `(matched, chars consumed)`, or
/// `None` when the class is not terminated.
fn match_class(p: &[char], c: char) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        let lo = if p[i] == '\\' && i + 1 < p.len() {
            i += 1;
            p[i]
        } else {
            p[i]
        };
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|&h| h != ']') {
            let hi = p[i + 2];
            if lo <= c && c <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if lo == c {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

/// `list_children` + glob filter on the name + offset pagination (`cursor` = decimal offset).
pub async fn children_page<S: Session + ?Sized>(
    s: &S,
    req: ChildrenPageRequest,
) -> Result<ChildrenPage> {
    let offset: usize = match req.cursor.as_deref() {
        None => 0,
        Some(c) => c
            .parse()
            .map_err(|_| Error::Other(format!("invalid cursor: {c}")))?,
    };
    let limit = req.limit.clamp(1, 5000) as usize;
    let mut nodes = s.list_children(&req.parent).await?;
    if let Some(pattern) = req.pattern.as_deref().filter(|p| !p.is_empty()) {
        nodes.retain(|n| glob_match(pattern, &n.name));
    }
    let total = nodes.len();
    let page: Vec<ObjectNode> = nodes.into_iter().skip(offset).take(limit).collect();
    let end = offset.saturating_add(page.len());
    Ok(ChildrenPage {
        nodes: page,
        next_cursor: (end < total).then(|| end.to_string()),
        approximate_total: Some(total as u64),
    })
}

fn is_read_only_column(c: &ColumnDetail) -> bool {
    let Some(extra) = c.extra.as_deref() else {
        return false;
    };
    let e = extra.to_lowercase();
    if e.contains("by default") {
        return false;
    }
    ["identity", "generated", "computed", "rowversion"]
        .iter()
        .any(|k| e.contains(k))
}

/// Editability derived from `table_details`: tables only; key = primary key, else the first
/// unique index whose columns are all NOT NULL; identity/generated/computed/rowversion columns
/// are read-only. A driver without `table_details` yields `Unsupported`.
pub async fn editability_from_details<S: Session + ?Sized>(
    s: &S,
    path: &ObjectPath,
) -> Result<Editability> {
    let not_editable = |reason| Editability {
        editable: false,
        key_columns: vec![],
        read_only_columns: vec![],
        reason: Some(reason),
        atomic: true,
    };
    let details = match s.table_details(path).await {
        Ok(d) => d,
        Err(Error::Unsupported(_)) => return Ok(not_editable(NotEditableReason::Unsupported)),
        Err(e) => return Err(e),
    };
    if details.kind != ObjectKind::Table {
        return Ok(not_editable(NotEditableReason::NotATable));
    }
    let read_only_columns = details
        .columns
        .iter()
        .filter(|c| is_read_only_column(c))
        .map(|c| c.name.clone())
        .collect();
    let key_columns = key_columns(&details);
    let editable = !key_columns.is_empty();
    Ok(Editability {
        editable,
        key_columns,
        read_only_columns,
        reason: (!editable).then_some(NotEditableReason::NoUniqueKey),
        atomic: true,
    })
}

fn primary_key(d: &TableDetails) -> Vec<String> {
    if let Some(ix) = d.indexes.iter().find(|i| i.primary) {
        return ix.columns.clone();
    }
    d.columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.clone())
        .collect()
}

fn key_columns(d: &TableDetails) -> Vec<String> {
    let pk = primary_key(d);
    if !pk.is_empty() {
        return pk;
    }
    d.indexes
        .iter()
        .filter(|i| i.unique && !i.primary && !i.columns.is_empty())
        .find(|i| {
            i.columns.iter().all(|name| {
                d.columns
                    .iter()
                    .find(|c| &c.name == name)
                    .is_some_and(|c| !c.nullable)
            })
        })
        .map(|i| i.columns.clone())
        .unwrap_or_default()
}

fn schema_name(path: &ObjectPath) -> Option<String> {
    path.segments
        .iter()
        .find(|s| s.kind == ObjectKind::Schema)
        .map(|s| s.name.clone())
}

/// Tables (and optionally views) under `scope`, found by walking `list_children`: folders
/// (`tables`, `views`, `materializedViews`, `schemas`) and `Schema` nodes are entered.
async fn collect_tables<S: Session + ?Sized>(
    s: &S,
    scope: &ObjectPath,
    include_views: bool,
) -> Result<Vec<ObjectPath>> {
    use crate::folder;
    let mut found = Vec::new();
    let mut queue = vec![(scope.clone(), 0usize)];
    while let Some((parent, depth)) = queue.pop() {
        for node in s.list_children(&parent).await? {
            match node.kind {
                ObjectKind::Table => found.push(node.path.clone()),
                ObjectKind::View | ObjectKind::MaterializedView if include_views => {
                    found.push(node.path.clone())
                }
                ObjectKind::Schema if depth < 2 => queue.push((node.path.clone(), depth + 1)),
                ObjectKind::Folder if depth < 3 => {
                    let wanted = match node.name.as_str() {
                        folder::TABLES | folder::SCHEMAS => true,
                        folder::VIEWS | folder::MATERIALIZED_VIEWS => include_views,
                        _ => false,
                    };
                    if wanted {
                        queue.push((node.path.clone(), depth + 1));
                    }
                }
                _ => {}
            }
            if found.len() > MAX_FALLBACK_TABLES {
                return Err(Error::Unsupported("schema_model".into()));
            }
        }
    }
    found.sort_by(|a, b| {
        let key = |p: &ObjectPath| {
            p.segments
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        };
        key(a).cmp(&key(b))
    });
    Ok(found)
}

/// Schema model from one `table_details` call per table (at most [`MAX_FALLBACK_TABLES`] tables,
/// else `Unsupported("schema_model")`). Relations come from the foreign keys.
pub async fn schema_model_via_details<S: Session + ?Sized>(
    s: &S,
    req: SchemaModelRequest,
) -> Result<SchemaModel> {
    let paths = if req.tables.is_empty() {
        collect_tables(s, &req.scope, req.include_views).await?
    } else {
        req.tables.clone()
    };
    if paths.len() > MAX_FALLBACK_TABLES {
        return Err(Error::Unsupported("schema_model".into()));
    }
    let mut tables = Vec::with_capacity(paths.len());
    let mut relations = Vec::new();
    for path in &paths {
        let d = s.table_details(path).await?;
        let pk = primary_key(&d);
        let unique_keys: Vec<Vec<String>> = d
            .indexes
            .iter()
            .filter(|i| i.unique && !i.primary)
            .map(|i| i.columns.clone())
            .collect();
        for fk in &d.foreign_keys {
            let nullable = |name: &String| {
                d.columns
                    .iter()
                    .find(|c| &c.name == name)
                    .is_none_or(|c| c.nullable)
            };
            let cols: BTreeSet<&String> = fk.columns.iter().collect();
            let same = |other: &Vec<String>| {
                !other.is_empty() && other.iter().collect::<BTreeSet<_>>() == cols
            };
            relations.push(Relation {
                name: fk.name.clone(),
                from_table: d.path.clone(),
                from_columns: fk.columns.clone(),
                to_table: fk.referenced_table.clone(),
                to_columns: fk.referenced_columns.clone(),
                on_update: fk.on_update.clone(),
                on_delete: fk.on_delete.clone(),
                from_optional: fk.columns.iter().any(nullable),
                one_to_one: same(&pk) || unique_keys.iter().any(same),
                inferred: false,
            });
        }
        let keep_all = req.detail == ModelDetail::Full;
        let fk_cols: BTreeSet<&String> = d.foreign_keys.iter().flat_map(|f| &f.columns).collect();
        let uniq_cols: BTreeSet<&String> = unique_keys.iter().flatten().collect();
        let columns = d
            .columns
            .iter()
            .filter(|c| {
                keep_all
                    || c.primary_key
                    || pk.contains(&c.name)
                    || fk_cols.contains(&c.name)
                    || uniq_cols.contains(&c.name)
            })
            .cloned()
            .map(|mut c| {
                if !req.include_comments {
                    c.comment = None;
                }
                c
            })
            .collect();
        tables.push(TableModel {
            path: d.path.clone(),
            kind: d.kind,
            schema: schema_name(&d.path),
            name: d
                .path
                .last()
                .map(|seg| seg.name.clone())
                .unwrap_or_default(),
            comment: if req.include_comments {
                d.comment.clone()
            } else {
                None
            },
            columns,
            primary_key: pk,
            unique_keys,
            estimated_rows: d.estimated_rows,
        });
    }
    Ok(SchemaModel {
        scope: req.scope,
        tables,
        relations,
        inferred: false,
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use uuid::Uuid;

    use super::*;
    use crate::{
        ExecSummary, ForeignKeyDetail, IndexDetail, QueryRequest, ResultSink, ServerInfo,
        ValueKind, folder,
    };

    /// Tree: `Database app > Folder tables > Table t*`, with canned details.
    #[derive(Default)]
    struct Mock {
        tables: Vec<String>,
        details: BTreeMap<String, TableDetails>,
        views: Vec<String>,
    }

    fn db() -> ObjectPath {
        ObjectPath::root().child(ObjectKind::Database, "app")
    }

    fn tpath(name: &str) -> ObjectPath {
        db().child(ObjectKind::Folder, folder::TABLES)
            .child(ObjectKind::Table, name)
    }

    fn col(name: &str, pk: bool, nullable: bool, extra: Option<&str>) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal: 1,
            type_name: "int8".into(),
            value_kind: ValueKind::Int,
            nullable,
            default: None,
            comment: Some("c".into()),
            primary_key: pk,
            extra: extra.map(String::from),
        }
    }

    fn idx(name: &str, cols: &[&str], unique: bool, primary: bool) -> IndexDetail {
        IndexDetail {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            unique,
            primary,
            method: None,
            definition: None,
        }
    }

    fn details(name: &str, columns: Vec<ColumnDetail>, indexes: Vec<IndexDetail>) -> TableDetails {
        TableDetails {
            path: tpath(name),
            kind: ObjectKind::Table,
            comment: Some("tc".into()),
            columns,
            indexes,
            foreign_keys: vec![],
            estimated_rows: Some(5),
            size_bytes: None,
        }
    }

    #[async_trait]
    impl Session for Mock {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn server_info(&self) -> Result<ServerInfo> {
            Ok(ServerInfo::default())
        }
        async fn execute(&self, _: QueryRequest, _: &mut dyn ResultSink) -> Result<ExecSummary> {
            Err(Error::Unsupported("execute".into()))
        }
        async fn list_children(&self, parent: &ObjectPath) -> Result<Vec<ObjectNode>> {
            let last = parent.last();
            let node = |path: ObjectPath, kind, name: &str, has_children| ObjectNode {
                path,
                kind,
                name: name.into(),
                has_children,
                detail: None,
            };
            match last {
                None => Ok(vec![node(db(), ObjectKind::Database, "app", true)]),
                Some(seg) if seg.kind == ObjectKind::Database => Ok(vec![
                    node(
                        db().child(ObjectKind::Folder, folder::TABLES),
                        ObjectKind::Folder,
                        folder::TABLES,
                        true,
                    ),
                    node(
                        db().child(ObjectKind::Folder, folder::VIEWS),
                        ObjectKind::Folder,
                        folder::VIEWS,
                        true,
                    ),
                    node(
                        db().child(ObjectKind::Folder, folder::ROLES),
                        ObjectKind::Folder,
                        folder::ROLES,
                        true,
                    ),
                ]),
                Some(seg) if seg.name == folder::TABLES => Ok(self
                    .tables
                    .iter()
                    .map(|t| node(tpath(t), ObjectKind::Table, t, true))
                    .collect()),
                Some(seg) if seg.name == folder::VIEWS => Ok(self
                    .views
                    .iter()
                    .map(|v| {
                        node(
                            db().child(ObjectKind::Folder, folder::VIEWS)
                                .child(ObjectKind::View, v),
                            ObjectKind::View,
                            v,
                            true,
                        )
                    })
                    .collect()),
                _ => Ok(vec![]),
            }
        }
        async fn table_details(&self, path: &ObjectPath) -> Result<TableDetails> {
            let name = &path.last().unwrap().name;
            if self.views.contains(name) {
                let mut d = details(name, vec![col("a", false, true, None)], vec![]);
                d.kind = ObjectKind::View;
                d.path = path.clone();
                return Ok(d);
            }
            self.details
                .get(name)
                .cloned()
                .ok_or_else(|| Error::NotFound(name.clone()))
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    /// A session without `table_details`.
    struct Bare;
    #[async_trait]
    impl Session for Bare {
        async fn ping(&self) -> Result<()> {
            Ok(())
        }
        async fn server_info(&self) -> Result<ServerInfo> {
            Ok(ServerInfo::default())
        }
        async fn execute(&self, _: QueryRequest, _: &mut dyn ResultSink) -> Result<ExecSummary> {
            Ok(ExecSummary::default())
        }
        async fn list_children(&self, _: &ObjectPath) -> Result<Vec<ObjectNode>> {
            Ok(vec![])
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
    }

    fn many(n: usize) -> Mock {
        let mut m = Mock::default();
        for i in 0..n {
            let name = format!("t{i:04}");
            m.details.insert(
                name.clone(),
                details(&name, vec![col("id", true, false, None)], vec![]),
            );
            m.tables.push(name);
        }
        m
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("user:*", "user:1:name"));
        assert!(glob_match("USER:*", "user:1"));
        assert!(!glob_match("user:*", "usr:1"));
        assert!(glob_match("h?llo", "hello"));
        assert!(!glob_match("h?llo", "hllo"));
        assert!(glob_match("h[ae]llo", "hallo"));
        assert!(!glob_match("h[^ae]llo", "hello"));
        assert!(glob_match("h[a-e]llo", "hello"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(glob_match("a\\*b", "a*b"));
        assert!(!glob_match("a\\*b", "aXb"));
        assert!(glob_match("[", "["));
    }

    #[tokio::test]
    async fn children_page_filters_and_paginates() {
        let mut m = Mock::default();
        for i in 0..7 {
            m.tables.push(format!("user{i}"));
        }
        m.tables.push("other".into());
        let parent = db().child(ObjectKind::Folder, folder::TABLES);
        let mut req = ChildrenPageRequest {
            parent: parent.clone(),
            cursor: None,
            limit: 3,
            pattern: Some("USER*".into()),
        };
        let p1 = children_page(&m, req.clone()).await.unwrap();
        assert_eq!(p1.nodes.len(), 3);
        assert_eq!(p1.next_cursor.as_deref(), Some("3"));
        assert_eq!(p1.approximate_total, Some(7));
        req.cursor = p1.next_cursor;
        let p2 = children_page(&m, req.clone()).await.unwrap();
        assert_eq!(p2.nodes[0].name, "user3");
        req.cursor = p2.next_cursor;
        let p3 = children_page(&m, req.clone()).await.unwrap();
        assert_eq!(p3.nodes.len(), 1);
        assert_eq!(p3.next_cursor, None);
        // limit is clamped to at least 1; no pattern returns everything.
        let all = children_page(
            &m,
            ChildrenPageRequest {
                parent,
                cursor: None,
                limit: 0,
                pattern: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(all.nodes.len(), 1);
        assert_eq!(all.next_cursor.as_deref(), Some("1"));
        assert_eq!(all.approximate_total, Some(8));
        req.cursor = Some("abc".into());
        assert!(children_page(&m, req).await.is_err());
    }

    #[tokio::test]
    async fn default_session_method_uses_fallback() {
        let m = Mock {
            tables: vec!["a".into(), "b".into()],
            ..Mock::default()
        };
        let page = m
            .list_children_page(ChildrenPageRequest {
                parent: db().child(ObjectKind::Folder, folder::TABLES),
                cursor: None,
                limit: 10,
                pattern: Some("a".into()),
            })
            .await
            .unwrap();
        assert_eq!(page.nodes.len(), 1);
        // Also callable through a trait object.
        let dynamic: &dyn Session = &m;
        let page = dynamic
            .list_children_page(ChildrenPageRequest {
                parent: db().child(ObjectKind::Folder, folder::TABLES),
                cursor: None,
                limit: 1,
                pattern: None,
            })
            .await
            .unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn editability_primary_key_and_read_only_columns() {
        let mut m = Mock::default();
        m.details.insert(
            "t".into(),
            details(
                "t",
                vec![
                    col("id", true, false, Some("identity")),
                    col("n", false, true, None),
                    col("g", false, true, Some("generated")),
                    col("d", false, true, Some("identity by default")),
                    col("rv", false, false, Some("rowversion")),
                ],
                vec![idx("t_pkey", &["id"], true, true)],
            ),
        );
        let e = editability_from_details(&m, &tpath("t")).await.unwrap();
        assert!(e.editable && e.atomic && e.reason.is_none());
        assert_eq!(e.key_columns, vec!["id"]);
        assert_eq!(e.read_only_columns, vec!["id", "g", "rv"]);
    }

    #[tokio::test]
    async fn editability_unique_not_null_fallback_and_none() {
        let mut m = Mock::default();
        m.details.insert(
            "u".into(),
            details(
                "u",
                vec![col("a", false, true, None), col("b", false, false, None)],
                vec![
                    idx("u_a", &["a"], true, false),
                    idx("u_b", &["b"], true, false),
                ],
            ),
        );
        m.details.insert(
            "n".into(),
            details(
                "n",
                vec![col("a", false, true, None)],
                vec![
                    idx("n_a", &["a"], true, false),
                    idx("n_x", &["a"], false, false),
                ],
            ),
        );
        let e = editability_from_details(&m, &tpath("u")).await.unwrap();
        assert!(e.editable);
        assert_eq!(e.key_columns, vec!["b"]);
        let e = editability_from_details(&m, &tpath("n")).await.unwrap();
        assert!(!e.editable);
        assert_eq!(e.reason, Some(NotEditableReason::NoUniqueKey));
        assert!(e.key_columns.is_empty());
    }

    #[tokio::test]
    async fn editability_views_and_unsupported() {
        let m = Mock {
            views: vec!["v".into()],
            ..Mock::default()
        };
        let view_path = db()
            .child(ObjectKind::Folder, folder::VIEWS)
            .child(ObjectKind::View, "v");
        let e = editability_from_details(&m, &view_path).await.unwrap();
        assert_eq!(e.reason, Some(NotEditableReason::NotATable));
        let e = Bare.editability(&tpath("x")).await.unwrap();
        assert!(!e.editable);
        assert_eq!(e.reason, Some(NotEditableReason::Unsupported));
        let err = editability_from_details(&m, &tpath("missing"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[tokio::test]
    async fn schema_model_collects_tables_and_relations() {
        let mut m = Mock::default();
        let mut orders = details(
            "orders",
            vec![
                col("id", true, false, None),
                col("user_id", false, true, None),
                col("note", false, true, None),
            ],
            vec![idx("orders_pkey", &["id"], true, true)],
        );
        orders.foreign_keys.push(ForeignKeyDetail {
            name: "fk_user".into(),
            columns: vec!["user_id".into()],
            referenced_table: tpath("users"),
            referenced_columns: vec!["id".into()],
            on_update: None,
            on_delete: Some("CASCADE".into()),
        });
        let mut profile = details(
            "profile",
            vec![col("user_id", true, false, None)],
            vec![idx("profile_pkey", &["user_id"], true, true)],
        );
        profile.foreign_keys.push(ForeignKeyDetail {
            name: "fk_profile".into(),
            columns: vec!["user_id".into()],
            referenced_table: tpath("users"),
            referenced_columns: vec!["id".into()],
            on_update: None,
            on_delete: None,
        });
        let users = details(
            "users",
            vec![
                col("id", true, false, None),
                col("email", false, false, None),
            ],
            vec![
                idx("users_pkey", &["id"], true, true),
                idx("users_email", &["email"], true, false),
            ],
        );
        for (n, d) in [("orders", orders), ("profile", profile), ("users", users)] {
            m.details.insert(n.into(), d);
            m.tables.push(n.into());
        }
        m.tables.reverse();
        let req = SchemaModelRequest {
            scope: db(),
            tables: vec![],
            include_views: false,
            include_comments: false,
            detail: ModelDetail::Full,
            sample_size: None,
        };
        let model = schema_model_via_details(&m, req.clone()).await.unwrap();
        let names: Vec<_> = model.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["orders", "profile", "users"], "sorted by path");
        assert!(!model.inferred && !model.truncated);
        assert_eq!(model.tables[0].primary_key, vec!["id"]);
        assert_eq!(model.tables[2].unique_keys, vec![vec!["email".to_string()]]);
        assert_eq!(model.tables[0].comment, None);
        assert!(model.tables[0].columns.iter().all(|c| c.comment.is_none()));
        assert_eq!(model.relations.len(), 2);
        let r = &model.relations[0];
        assert_eq!(r.name, "fk_user");
        assert!(r.from_optional && !r.one_to_one && !r.inferred);
        assert_eq!(r.on_delete.as_deref(), Some("CASCADE"));
        let r = &model.relations[1];
        assert!(!r.from_optional && r.one_to_one);

        // Comments and detail level.
        let with = schema_model_via_details(
            &m,
            SchemaModelRequest {
                include_comments: true,
                detail: ModelDetail::Keys,
                ..req.clone()
            },
        )
        .await
        .unwrap();
        assert_eq!(with.tables[0].comment.as_deref(), Some("tc"));
        let cols: Vec<_> = with.tables[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cols, vec!["id", "user_id"], "note is not a key column");

        // Via the default Session method with an explicit table list.
        let one = m
            .schema_model(SchemaModelRequest {
                tables: vec![tpath("users")],
                ..req
            })
            .await
            .unwrap();
        assert_eq!(one.tables.len(), 1);
    }

    #[tokio::test]
    async fn schema_model_includes_views_on_request() {
        let mut m = many(1);
        m.views = vec!["v1".into()];
        let req = SchemaModelRequest {
            scope: db(),
            tables: vec![],
            include_views: true,
            include_comments: false,
            detail: ModelDetail::Full,
            sample_size: None,
        };
        let model = schema_model_via_details(&m, req.clone()).await.unwrap();
        assert_eq!(model.tables.len(), 2);
        assert!(model.tables.iter().any(|t| t.kind == ObjectKind::View));
        let model = schema_model_via_details(
            &m,
            SchemaModelRequest {
                include_views: false,
                ..req
            },
        )
        .await
        .unwrap();
        assert_eq!(model.tables.len(), 1);
    }

    #[tokio::test]
    async fn schema_model_limit_is_300_tables() {
        let ok = many(MAX_FALLBACK_TABLES);
        let req = SchemaModelRequest {
            scope: db(),
            tables: vec![],
            include_views: false,
            include_comments: false,
            detail: ModelDetail::Keys,
            sample_size: None,
        };
        let model = schema_model_via_details(&ok, req.clone()).await.unwrap();
        assert_eq!(model.tables.len(), 300);
        let too_many = many(MAX_FALLBACK_TABLES + 1);
        let err = schema_model_via_details(&too_many, req).await.unwrap_err();
        assert!(matches!(err, Error::Unsupported(c) if c == "schema_model"));
        // Explicit table lists are limited as well.
        let err = too_many
            .schema_model(SchemaModelRequest {
                scope: db(),
                tables: too_many.tables.iter().map(|t| tpath(t)).collect(),
                include_views: false,
                include_comments: false,
                detail: ModelDetail::Keys,
                sample_size: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
        let _ = Uuid::nil();
    }
}
