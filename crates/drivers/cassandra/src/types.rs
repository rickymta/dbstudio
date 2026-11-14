//! CQL type model: parsing of the type strings found in `system_schema`, conversion from the
//! driver's `ColumnType`, display, and the closest generic [`ValueKind`].

use std::collections::BTreeMap;
use std::fmt;

use dbstudio_core::ValueKind;
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};

/// A CQL data type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CqlType {
    Ascii,
    Boolean,
    Blob,
    Counter,
    Date,
    Decimal,
    Double,
    Duration,
    Float,
    Int,
    BigInt,
    Text,
    Timestamp,
    Inet,
    SmallInt,
    TinyInt,
    Time,
    Timeuuid,
    Uuid,
    Varint,
    List(Box<CqlType>),
    Set(Box<CqlType>),
    Map(Box<CqlType>, Box<CqlType>),
    Vector(Box<CqlType>, u32),
    Tuple(Vec<CqlType>),
    /// User-defined type. `fields` is empty when only the name is known (parsed from a string).
    Udt {
        name: String,
        fields: Vec<(String, CqlType)>,
    },
    /// `frozen<...>` wrapper (kept for display; transparent everywhere else).
    Frozen(Box<CqlType>),
    /// Anything else (custom Java class names...).
    Custom(String),
}

impl CqlType {
    /// Remove `frozen<>` wrappers.
    pub(crate) fn unfrozen(&self) -> &CqlType {
        match self {
            CqlType::Frozen(inner) => inner.unfrozen(),
            other => other,
        }
    }

    /// Parse a type as printed by `system_schema` (`frozen<map<text, list<int>>>`, `address`,
    /// `vector<float, 3>`, `"MyType"`).
    pub(crate) fn parse(text: &str) -> CqlType {
        let mut p = Parser {
            s: text.trim(),
            pos: 0,
        };
        p.parse_type()
    }

    /// Closest generic kind of the cells of this type.
    pub(crate) fn value_kind(&self) -> ValueKind {
        match self.unfrozen() {
            CqlType::Ascii | CqlType::Text | CqlType::Inet => ValueKind::Text,
            CqlType::Boolean => ValueKind::Bool,
            CqlType::Blob => ValueKind::Bytes,
            CqlType::Counter
            | CqlType::BigInt
            | CqlType::Int
            | CqlType::SmallInt
            | CqlType::TinyInt => ValueKind::Int,
            CqlType::Date => ValueKind::Date,
            CqlType::Decimal | CqlType::Varint => ValueKind::Decimal,
            CqlType::Double | CqlType::Float => ValueKind::Float,
            CqlType::Duration => ValueKind::Ext,
            CqlType::Timestamp => ValueKind::Timestamp,
            CqlType::Time => ValueKind::Time,
            CqlType::Timeuuid | CqlType::Uuid => ValueKind::Uuid,
            CqlType::List(_) | CqlType::Set(_) | CqlType::Vector(..) | CqlType::Tuple(_) => {
                ValueKind::Array
            }
            CqlType::Map(k, _) => {
                if matches!(k.unfrozen(), CqlType::Text | CqlType::Ascii) {
                    ValueKind::Document
                } else {
                    ValueKind::Array
                }
            }
            CqlType::Udt { .. } => ValueKind::Document,
            CqlType::Frozen(_) => ValueKind::Null,
            CqlType::Custom(_) => ValueKind::Bytes,
        }
    }

    /// Whether a map of this key type becomes a `Document` (text keys).
    pub(crate) fn is_text(&self) -> bool {
        matches!(self.unfrozen(), CqlType::Text | CqlType::Ascii)
    }

    /// Native type name in CQL (`int`, `bigint`...), `None` for composite types.
    pub(crate) fn native_name(&self) -> Option<&'static str> {
        Some(match self.unfrozen() {
            CqlType::Ascii => "ascii",
            CqlType::Boolean => "boolean",
            CqlType::Blob => "blob",
            CqlType::Counter => "counter",
            CqlType::Date => "date",
            CqlType::Decimal => "decimal",
            CqlType::Double => "double",
            CqlType::Duration => "duration",
            CqlType::Float => "float",
            CqlType::Int => "int",
            CqlType::BigInt => "bigint",
            CqlType::Text => "text",
            CqlType::Timestamp => "timestamp",
            CqlType::Inet => "inet",
            CqlType::SmallInt => "smallint",
            CqlType::TinyInt => "tinyint",
            CqlType::Time => "time",
            CqlType::Timeuuid => "timeuuid",
            CqlType::Uuid => "uuid",
            CqlType::Varint => "varint",
            _ => return None,
        })
    }

    /// Names of the user-defined types this type refers to (for dependency ordering).
    pub(crate) fn udt_names(&self, out: &mut Vec<String>) {
        match self {
            CqlType::List(t) | CqlType::Set(t) | CqlType::Frozen(t) | CqlType::Vector(t, _) => {
                t.udt_names(out)
            }
            CqlType::Map(k, v) => {
                k.udt_names(out);
                v.udt_names(out);
            }
            CqlType::Tuple(items) => items.iter().for_each(|t| t.udt_names(out)),
            CqlType::Udt { name, .. } => out.push(name.clone()),
            _ => {}
        }
    }

    /// Fill in the field definitions of user-defined types from `defs` (type name to fields).
    pub(crate) fn resolved(&self, defs: &BTreeMap<String, Vec<(String, CqlType)>>) -> CqlType {
        match self {
            CqlType::List(t) => CqlType::List(Box::new(t.resolved(defs))),
            CqlType::Set(t) => CqlType::Set(Box::new(t.resolved(defs))),
            CqlType::Frozen(t) => CqlType::Frozen(Box::new(t.resolved(defs))),
            CqlType::Vector(t, n) => CqlType::Vector(Box::new(t.resolved(defs)), *n),
            CqlType::Map(k, v) => {
                CqlType::Map(Box::new(k.resolved(defs)), Box::new(v.resolved(defs)))
            }
            CqlType::Tuple(items) => {
                CqlType::Tuple(items.iter().map(|t| t.resolved(defs)).collect())
            }
            CqlType::Udt { name, fields } if fields.is_empty() => match defs.get(name) {
                Some(fs) => CqlType::Udt {
                    name: name.clone(),
                    fields: fs
                        .iter()
                        .map(|(n, t)| (n.clone(), t.resolved(defs)))
                        .collect(),
                },
                None => self.clone(),
            },
            other => other.clone(),
        }
    }

    /// Convert the driver's column type.
    pub(crate) fn from_column_type(ty: &ColumnType<'_>) -> CqlType {
        match ty {
            ColumnType::Native(n) => match n {
                NativeType::Ascii => CqlType::Ascii,
                NativeType::Boolean => CqlType::Boolean,
                NativeType::Blob => CqlType::Blob,
                NativeType::Counter => CqlType::Counter,
                NativeType::Date => CqlType::Date,
                NativeType::Decimal => CqlType::Decimal,
                NativeType::Double => CqlType::Double,
                NativeType::Duration => CqlType::Duration,
                NativeType::Float => CqlType::Float,
                NativeType::Int => CqlType::Int,
                NativeType::BigInt => CqlType::BigInt,
                NativeType::Text => CqlType::Text,
                NativeType::Timestamp => CqlType::Timestamp,
                NativeType::Inet => CqlType::Inet,
                NativeType::SmallInt => CqlType::SmallInt,
                NativeType::TinyInt => CqlType::TinyInt,
                NativeType::Time => CqlType::Time,
                NativeType::Timeuuid => CqlType::Timeuuid,
                NativeType::Uuid => CqlType::Uuid,
                NativeType::Varint => CqlType::Varint,
                other => CqlType::Custom(other.to_string()),
            },
            ColumnType::Collection { typ, .. } => match typ {
                CollectionType::List(t) => CqlType::List(Box::new(Self::from_column_type(t))),
                CollectionType::Set(t) => CqlType::Set(Box::new(Self::from_column_type(t))),
                CollectionType::Map(k, v) => CqlType::Map(
                    Box::new(Self::from_column_type(k)),
                    Box::new(Self::from_column_type(v)),
                ),
                _ => CqlType::Custom("collection".into()),
            },
            ColumnType::Vector { typ, dimensions } => CqlType::Vector(
                Box::new(Self::from_column_type(typ)),
                u32::from(*dimensions),
            ),
            ColumnType::UserDefinedType { definition, .. } => CqlType::Udt {
                name: definition.name.to_string(),
                fields: definition
                    .field_types
                    .iter()
                    .map(|(n, t)| (n.to_string(), Self::from_column_type(t)))
                    .collect(),
            },
            ColumnType::Tuple(items) => {
                CqlType::Tuple(items.iter().map(Self::from_column_type).collect())
            }
            _ => CqlType::Custom("unknown".into()),
        }
    }
}

impl fmt::Display for CqlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(n) = self.native_name() {
            if !matches!(self, CqlType::Frozen(_)) {
                return f.write_str(n);
            }
        }
        match self {
            CqlType::List(t) => write!(f, "list<{t}>"),
            CqlType::Set(t) => write!(f, "set<{t}>"),
            CqlType::Map(k, v) => write!(f, "map<{k}, {v}>"),
            CqlType::Vector(t, n) => write!(f, "vector<{t}, {n}>"),
            CqlType::Tuple(items) => {
                f.write_str("tuple<")?;
                for (i, t) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{t}")?;
                }
                f.write_str(">")
            }
            CqlType::Udt { name, .. } => f.write_str(&crate::ident::quote_ident(name)),
            CqlType::Frozen(t) => write!(f, "frozen<{t}>"),
            CqlType::Custom(s) => f.write_str(s),
            _ => f.write_str(self.native_name().unwrap_or("?")),
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.s[self.pos..]
    }

    fn skip_ws(&mut self) {
        while self.rest().starts_with(char::is_whitespace) {
            self.pos += self.rest().chars().next().map_or(1, char::len_utf8);
        }
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.rest().starts_with(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> String {
        self.skip_ws();
        if self.rest().starts_with('"') {
            // Quoted identifier with `""` escapes.
            let mut out = String::new();
            let mut chars = self.rest()[1..].char_indices().peekable();
            let mut consumed = 1;
            while let Some((i, c)) = chars.next() {
                consumed = 1 + i + c.len_utf8();
                if c == '"' {
                    if matches!(chars.peek(), Some((_, '"'))) {
                        out.push('"');
                        let (j, c2) = chars.next().unwrap_or((i, '"'));
                        consumed = 1 + j + c2.len_utf8();
                    } else {
                        break;
                    }
                } else {
                    out.push(c);
                }
            }
            self.pos += consumed;
            return out;
        }
        let end = self
            .rest()
            .find(|c: char| matches!(c, '<' | '>' | ',' | '(' | ')') || c.is_whitespace())
            .unwrap_or(self.rest().len());
        let id = self.rest()[..end].to_string();
        self.pos += end;
        id
    }

    fn parse_type(&mut self) -> CqlType {
        let name = self.ident();
        let lower = name.to_ascii_lowercase();
        let simple = |t: CqlType| t;
        match lower.as_str() {
            "ascii" => simple(CqlType::Ascii),
            "boolean" => CqlType::Boolean,
            "blob" => CqlType::Blob,
            "counter" => CqlType::Counter,
            "date" => CqlType::Date,
            "decimal" => CqlType::Decimal,
            "double" => CqlType::Double,
            "duration" => CqlType::Duration,
            "float" => CqlType::Float,
            "int" => CqlType::Int,
            "bigint" => CqlType::BigInt,
            "text" | "varchar" => CqlType::Text,
            "timestamp" => CqlType::Timestamp,
            "inet" => CqlType::Inet,
            "smallint" => CqlType::SmallInt,
            "tinyint" => CqlType::TinyInt,
            "time" => CqlType::Time,
            "timeuuid" => CqlType::Timeuuid,
            "uuid" => CqlType::Uuid,
            "varint" => CqlType::Varint,
            "frozen" | "list" | "set" | "map" | "vector" | "tuple" if self.eat('<') => {
                let ty = match lower.as_str() {
                    "frozen" => CqlType::Frozen(Box::new(self.parse_type())),
                    "list" => CqlType::List(Box::new(self.parse_type())),
                    "set" => CqlType::Set(Box::new(self.parse_type())),
                    "map" => {
                        let k = self.parse_type();
                        self.eat(',');
                        CqlType::Map(Box::new(k), Box::new(self.parse_type()))
                    }
                    "vector" => {
                        let t = self.parse_type();
                        self.eat(',');
                        let n = self.ident().parse().unwrap_or(0);
                        CqlType::Vector(Box::new(t), n)
                    }
                    _ => {
                        let mut items = vec![self.parse_type()];
                        while self.eat(',') {
                            items.push(self.parse_type());
                        }
                        CqlType::Tuple(items)
                    }
                };
                self.eat('>');
                ty
            }
            _ if name.contains('.') && !name.starts_with('"') => CqlType::Custom(name),
            _ => CqlType::Udt {
                name,
                fields: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_types() {
        let t = CqlType::parse("frozen<map<text, frozen<list<int>>>>");
        assert_eq!(t.to_string(), "frozen<map<text, frozen<list<int>>>>");
        assert_eq!(t.value_kind(), ValueKind::Document);
        let m = CqlType::parse("map<int, text>");
        assert_eq!(m.value_kind(), ValueKind::Array);
        let v = CqlType::parse("vector<float, 3>");
        assert_eq!(v, CqlType::Vector(Box::new(CqlType::Float), 3));
        let tup = CqlType::parse("tuple<int, text, frozen<address>>");
        assert_eq!(tup.to_string(), "tuple<int, text, frozen<address>>");
    }

    #[test]
    fn udt_and_custom() {
        assert!(matches!(CqlType::parse("address"), CqlType::Udt { .. }));
        assert_eq!(CqlType::parse("\"My Type\"").to_string(), "\"My Type\"");
        assert!(matches!(
            CqlType::parse("org.apache.cassandra.db.marshal.FooType"),
            CqlType::Custom(_)
        ));
        let mut names = Vec::new();
        CqlType::parse("list<frozen<address>>").udt_names(&mut names);
        assert_eq!(names, vec!["address".to_string()]);
    }

    #[test]
    fn resolves_udt_fields() {
        let mut defs = BTreeMap::new();
        defs.insert(
            "address".to_string(),
            vec![("zip".to_string(), CqlType::Int)],
        );
        match CqlType::parse("frozen<address>").resolved(&defs) {
            CqlType::Frozen(inner) => match *inner {
                CqlType::Udt { fields, .. } => assert_eq!(fields.len(), 1),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }
}
