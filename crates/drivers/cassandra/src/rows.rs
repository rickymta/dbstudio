//! Untyped query results (system tables, small results) and generic helpers on them.

use std::collections::BTreeMap;

use dbstudio_core::Result;
use scylla::client::session::Session;
use scylla::value::{CqlValue, Row as ScyllaRow};

use crate::error;

/// One row by column name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Row {
    pub(crate) cols: Vec<(String, Option<CqlValue>)>,
}

impl Row {
    pub(crate) fn get(&self, name: &str) -> Option<&CqlValue> {
        self.cols
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_ref())
    }

    pub(crate) fn str(&self, name: &str) -> Option<String> {
        match self.get(name)? {
            CqlValue::Text(s) | CqlValue::Ascii(s) => Some(s.clone()),
            _ => None,
        }
    }

    pub(crate) fn string(&self, name: &str) -> String {
        self.str(name).unwrap_or_default()
    }

    pub(crate) fn bool(&self, name: &str) -> Option<bool> {
        self.get(name)?.as_boolean()
    }

    pub(crate) fn int(&self, name: &str) -> Option<i64> {
        match self.get(name)? {
            CqlValue::Int(i) => Some(i64::from(*i)),
            CqlValue::BigInt(i) => Some(*i),
            CqlValue::SmallInt(i) => Some(i64::from(*i)),
            CqlValue::TinyInt(i) => Some(i64::from(*i)),
            CqlValue::Counter(c) => Some(c.0),
            _ => None,
        }
    }

    pub(crate) fn text_list(&self, name: &str) -> Vec<String> {
        match self.get(name) {
            Some(CqlValue::List(items) | CqlValue::Set(items)) => items
                .iter()
                .filter_map(|v| match v {
                    CqlValue::Text(s) | CqlValue::Ascii(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn text_map(&self, name: &str) -> BTreeMap<String, String> {
        match self.get(name) {
            Some(CqlValue::Map(pairs)) => pairs
                .iter()
                .filter_map(|(k, v)| match (k, v) {
                    (
                        CqlValue::Text(k) | CqlValue::Ascii(k),
                        CqlValue::Text(v) | CqlValue::Ascii(v),
                    ) => Some((k.clone(), v.clone())),
                    _ => None,
                })
                .collect(),
            _ => BTreeMap::new(),
        }
    }
}

/// Run `cql` unpaged and return every row by column name.
pub(crate) async fn query_rows(session: &Session, cql: &str) -> Result<Vec<Row>> {
    let result = session
        .query_unpaged(cql, &[])
        .await
        .map_err(|e| error::from_execution(&e, cql))?;
    if !result.is_rows() {
        return Ok(Vec::new());
    }
    let rows = result
        .into_rows_result()
        .map_err(|e| dbstudio_core::Error::Other(e.to_string()))?;
    let names: Vec<String> = rows
        .column_specs()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let iter = rows
        .rows::<ScyllaRow>()
        .map_err(|e| dbstudio_core::Error::Other(e.to_string()))?;
    let mut out = Vec::new();
    for r in iter {
        let r = r.map_err(|e| dbstudio_core::Error::Other(e.to_string()))?;
        out.push(Row {
            cols: names.iter().cloned().zip(r.columns).collect(),
        });
    }
    Ok(out)
}
