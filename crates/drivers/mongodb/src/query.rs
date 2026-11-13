//! Table-browser requests (filters, sort) translated to MongoDB filter and sort documents.

use bson::{Bson, Document, doc};
use dbstudio_core::{ColumnFilter, FilterOp, ObjectKind, ObjectPath, Result, SortKey};

use crate::shell::{parse_filter_text, parse_literal};

/// Escape regex metacharacters so user text matches literally.
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}-/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn value_of(text: &str) -> Bson {
    parse_literal(text).unwrap_or_else(|| Bson::String(text.to_string()))
}

fn regex(pattern: String) -> Bson {
    match (
        bson::raw::CString::try_from(pattern),
        bson::raw::CString::try_from("i".to_string()),
    ) {
        (Ok(pattern), Ok(options)) => Bson::RegularExpression(bson::Regex { pattern, options }),
        _ => Bson::Null,
    }
}

fn one(f: &ColumnFilter) -> Document {
    let c = f.column.as_str();
    let v0 = f.values.first().map(String::as_str).unwrap_or("");
    match f.op {
        FilterOp::Eq => doc! {c: value_of(v0)},
        FilterOp::Ne => doc! {c: {"$ne": value_of(v0)}},
        FilterOp::Lt => doc! {c: {"$lt": value_of(v0)}},
        FilterOp::Le => doc! {c: {"$lte": value_of(v0)}},
        FilterOp::Gt => doc! {c: {"$gt": value_of(v0)}},
        FilterOp::Ge => doc! {c: {"$gte": value_of(v0)}},
        FilterOp::Contains => doc! {c: regex(regex_escape(v0))},
        FilterOp::NotContains => {
            doc! {c: {"$not": regex(regex_escape(v0))}, "$and": [{c: {"$ne": Bson::Null}}]}
        }
        FilterOp::StartsWith => doc! {c: regex(format!("^{}", regex_escape(v0)))},
        FilterOp::EndsWith => doc! {c: regex(format!("{}$", regex_escape(v0)))},
        FilterOp::IsNull => doc! {c: Bson::Null},
        FilterOp::IsNotNull => doc! {c: {"$ne": Bson::Null}},
        FilterOp::In => {
            doc! {c: {"$in": f.values.iter().map(|v| value_of(v)).collect::<Vec<_>>()}}
        }
    }
}

/// Filter document of `filters` (AND) plus `raw_where` (EJSON / shell object literal).
pub fn build_filter(filters: &[ColumnFilter], raw_where: Option<&str>) -> Result<Document> {
    let mut parts: Vec<Document> = filters.iter().map(one).collect();
    if let Some(raw) = raw_where {
        let d = parse_filter_text(raw)?;
        if !d.is_empty() {
            parts.push(d);
        }
    }
    Ok(match parts.len() {
        0 => Document::new(),
        1 => parts.remove(0),
        _ => doc! {"$and": parts.into_iter().map(Bson::Document).collect::<Vec<_>>()},
    })
}

/// Sort document; `_id` is appended so that skip/limit paging is deterministic.
pub fn build_sort(sort: &[SortKey], with_key: bool) -> Document {
    let mut d = Document::new();
    for s in sort {
        if !d.contains_key(&s.column) {
            d.insert(s.column.clone(), if s.descending { -1 } else { 1 });
        }
    }
    if with_key && !d.contains_key("_id") {
        d.insert("_id", 1);
    }
    d
}

/// Database and collection (or view) of a path, with or without folder segments.
pub fn resolve(path: &ObjectPath) -> Option<(String, Option<(String, ObjectKind)>)> {
    let db = path
        .segments
        .iter()
        .find(|s| s.kind == ObjectKind::Database)?
        .name
        .clone();
    let coll = path
        .segments
        .iter()
        .find(|s| {
            matches!(
                s.kind,
                ObjectKind::Collection | ObjectKind::View | ObjectKind::Table
            )
        })
        .map(|s| (s.name.clone(), s.kind));
    Some((db, coll))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(col: &str, op: FilterOp, v: &[&str]) -> ColumnFilter {
        ColumnFilter {
            column: col.into(),
            op,
            values: v.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn filters_translate() {
        let d = build_filter(&[f("age", FilterOp::Gt, &["30"])], None).unwrap();
        assert_eq!(d, doc! {"age": {"$gt": 30}});
        let d = build_filter(&[f("a.b", FilterOp::Eq, &["hello"])], None).unwrap();
        assert_eq!(d, doc! {"a.b": "hello"});
        let d = build_filter(&[f("n", FilterOp::Contains, &["a.b"])], None).unwrap();
        match d.get("n") {
            Some(Bson::RegularExpression(r)) => assert_eq!(r.pattern.to_string(), "a\\.b"),
            other => panic!("{other:?}"),
        }
        let d = build_filter(
            &[f("x", FilterOp::IsNull, &[])],
            Some("{\"y\": {\"$exists\": true}}"),
        )
        .unwrap();
        assert_eq!(d.get_array("$and").unwrap().len(), 2);
        assert!(build_filter(&[], Some("{bad")).is_err());
    }

    #[test]
    fn sort_appends_id() {
        let s = build_sort(
            &[SortKey {
                column: "name".into(),
                descending: true,
            }],
            true,
        );
        assert_eq!(s, doc! {"name": -1, "_id": 1});
    }
}
