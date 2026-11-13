use serde::{Deserialize, Serialize};

/// Kind of a node in the object tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ObjectKind {
    /// Database, keyspace or numbered DB.
    Database,
    /// Schema (namespace inside a database).
    Schema,
    /// Table.
    Table,
    /// View.
    View,
    /// Materialized view.
    MaterializedView,
    /// Database role or user.
    Role,
    /// Table column or document field.
    Column,
    /// Index.
    Index,
    /// Stored function.
    Function,
    /// Stored procedure.
    Procedure,
    /// Trigger.
    Trigger,
    /// Sequence.
    Sequence,
    /// Document collection.
    Collection,
    /// Key in a key-value store.
    Key,
    /// Purely organisational grouping node (e.g. "Tables").
    Folder,
    /// Redis virtual folder; `name` is the full prefix including the delimiter (`"user:1:"`).
    KeyPrefix,
    /// Synonym (SQL Server, Oracle).
    Synonym,
    /// Oracle package (spec and body shown as one node; DDL gives both).
    Package,
    /// User-defined type (PostgreSQL, SQL Server, Oracle); optional for drivers.
    Type,
}

/// Stable keys used as the `name` of [`ObjectKind::Folder`] path segments. The UI translates
/// these keys, so they must never be localized or renamed.
pub mod folder {
    /// Tables folder.
    pub const TABLES: &str = "tables";
    /// Views folder.
    pub const VIEWS: &str = "views";
    /// Materialized views folder.
    pub const MATERIALIZED_VIEWS: &str = "materializedViews";
    /// Functions folder.
    pub const FUNCTIONS: &str = "functions";
    /// Procedures folder.
    pub const PROCEDURES: &str = "procedures";
    /// Triggers folder.
    pub const TRIGGERS: &str = "triggers";
    /// Sequences folder.
    pub const SEQUENCES: &str = "sequences";
    /// Indexes folder.
    pub const INDEXES: &str = "indexes";
    /// Columns folder.
    pub const COLUMNS: &str = "columns";
    /// Roles folder.
    pub const ROLES: &str = "roles";
    /// Schemas folder (databases of schema-capable servers).
    pub const SCHEMAS: &str = "schemas";
    /// Collections folder (MongoDB).
    pub const COLLECTIONS: &str = "collections";
    /// Synonyms folder.
    pub const SYNONYMS: &str = "synonyms";
    /// Packages folder (Oracle).
    pub const PACKAGES: &str = "packages";
    /// User-defined types folder.
    pub const TYPES: &str = "types";
}

/// One step of an [`ObjectPath`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathSegment {
    /// Kind of the object at this level.
    pub kind: ObjectKind,
    /// Name of the object at this level.
    pub name: String,
}

/// Location of a node in the object tree. The empty path is the server root.
///
/// Paths mirror the tree exactly, including [`ObjectKind::Folder`] segments: the path of a node
/// is the path of its parent (the path `list_children` was called with) plus the node's own
/// segment, e.g. `[Database db, Folder tables, Table t, Folder columns, Column c]`. Drivers must
/// also accept paths with or without Folder segments in every method.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectPath {
    /// Segments from the root down to the node.
    pub segments: Vec<PathSegment>,
}

impl ObjectPath {
    /// The server root.
    pub fn root() -> Self {
        Self::default()
    }

    /// True for the server root.
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// Path of a child of `self`.
    pub fn child(&self, kind: ObjectKind, name: impl Into<String>) -> Self {
        let mut segments = self.segments.clone();
        segments.push(PathSegment {
            kind,
            name: name.into(),
        });
        ObjectPath { segments }
    }

    /// Last segment, if any.
    pub fn last(&self) -> Option<&PathSegment> {
        self.segments.last()
    }
}

/// A node returned when browsing the object tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectNode {
    /// Full path of the node: the parent's path plus this node's own segment.
    pub path: ObjectPath,
    /// Kind of the node.
    pub kind: ObjectKind,
    /// Display name.
    pub name: String,
    /// Whether [`crate::Session::list_children`] may return something for this node.
    pub has_children: bool,
    /// Short extra info such as a type name or row estimate.
    pub detail: Option<String>,
}

/// Request for one page of children, see [`crate::Session::list_children_page`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildrenPageRequest {
    /// Node whose children are listed.
    pub parent: ObjectPath,
    /// Opaque cursor returned by the previous page; `None` for the first page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum nodes in the page (clamped to `1..=5000`).
    pub limit: u32,
    /// Glob (`*`, `?`, `[...]`) on the child name; Redis passes it to `SCAN MATCH`.
    #[serde(default)]
    pub pattern: Option<String>,
}

/// One page of children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildrenPage {
    /// Nodes of this page.
    pub nodes: Vec<ObjectNode>,
    /// Cursor of the next page; `None` when done.
    pub next_cursor: Option<String>,
    /// Estimated number of children in total, when known.
    pub approximate_total: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_building_and_serde() {
        let p = ObjectPath::root()
            .child(ObjectKind::Database, "app")
            .child(ObjectKind::Table, "t");
        assert!(!p.is_root());
        assert_eq!(p.last().unwrap().name, "t");
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["segments"][0]["kind"], "database");
        assert_eq!(serde_json::from_value::<ObjectPath>(json).unwrap(), p);
    }

    #[test]
    fn new_kinds_serialize_camel_case() {
        assert_eq!(
            serde_json::to_value(ObjectKind::MaterializedView).unwrap(),
            "materializedView"
        );
        assert_eq!(serde_json::to_value(ObjectKind::Role).unwrap(), "role");
        assert_eq!(folder::MATERIALIZED_VIEWS, "materializedViews");
    }

    #[test]
    fn phase2_kinds_and_folders() {
        assert_eq!(
            serde_json::to_value(ObjectKind::KeyPrefix).unwrap(),
            "keyPrefix"
        );
        assert_eq!(
            serde_json::to_value(ObjectKind::Synonym).unwrap(),
            "synonym"
        );
        assert_eq!(
            serde_json::to_value(ObjectKind::Package).unwrap(),
            "package"
        );
        assert_eq!(serde_json::to_value(ObjectKind::Type).unwrap(), "type");
        assert_eq!(folder::COLLECTIONS, "collections");
        assert_eq!(folder::SYNONYMS, "synonyms");
        assert_eq!(folder::PACKAGES, "packages");
        assert_eq!(folder::TYPES, "types");
    }

    #[test]
    fn children_page_shapes() {
        let req = ChildrenPageRequest {
            parent: ObjectPath::root().child(ObjectKind::Database, "0"),
            cursor: None,
            limit: 100,
            pattern: Some("user:*".into()),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["limit"], 100);
        assert_eq!(json["pattern"], "user:*");
        assert_eq!(
            serde_json::from_value::<ChildrenPageRequest>(json).unwrap(),
            req
        );
        let omitted: ChildrenPageRequest =
            serde_json::from_value(serde_json::json!({"parent": {"segments": []}, "limit": 5}))
                .unwrap();
        assert_eq!(omitted.cursor, None);
        let page = ChildrenPage {
            nodes: vec![],
            next_cursor: Some("17".into()),
            approximate_total: Some(1000),
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["nextCursor"], "17");
        assert_eq!(json["approximateTotal"], 1000);
        assert_eq!(serde_json::from_value::<ChildrenPage>(json).unwrap(), page);
    }
}
