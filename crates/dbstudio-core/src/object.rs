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
    /// View or materialized view.
    View,
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
    /// Full path of the node.
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
}
