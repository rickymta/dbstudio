//! Helpers for the JSON contract fixtures (`contract/fixtures/<group>.json` at the repository
//! root). Enabled by the `testing` cargo feature; other crates use it from their tests through
//! `dbstudio-core = { workspace = true, features = ["testing"] }` in `[dev-dependencies]`.
//!
//! Each fixture file is a JSON object mapping a name (the Rust type name, except in the `value`
//! and `error` groups) to a sample value. The same files are read by the frontend contract test
//! (`src/lib/types/contract.test.ts`).
//!
//! Several crates contribute to one group, so the helper **merges**: with `UPDATE_FIXTURES=1` it
//! overwrites only the given names; otherwise it asserts that every given name is present with
//! exactly the freshly serialised value (a missing file/name or a different shape fails the test).

use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value};

/// Directory holding the fixtures (`<repo>/contract/fixtures`).
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contract/fixtures")
}

/// Sorts object keys recursively so the files are stable regardless of serde_json features.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Whether fixtures are being (re)generated (`UPDATE_FIXTURES=1`).
pub fn updating() -> bool {
    std::env::var("UPDATE_FIXTURES").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Write (merge into) or check `contract/fixtures/<group>.json` with `samples`, a JSON object of
/// name to sample. Panics with an explanation on drift.
pub fn write_or_check(group: &str, samples: &Value) {
    let new = samples
        .as_object()
        .unwrap_or_else(|| panic!("fixture group {group}: samples must be a JSON object"));
    let path = fixtures_dir().join(format!("{group}.json"));
    let existing: Option<Map<String, Value>> =
        fs::read_to_string(&path)
            .ok()
            .map(|text| match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(m)) => m,
                _ => panic!("{} is not a JSON object", path.display()),
            });
    if updating() {
        let mut merged = existing.unwrap_or_default();
        for (name, sample) in new {
            merged.insert(name.clone(), sample.clone());
        }
        fs::create_dir_all(path.parent().unwrap()).expect("create contract/fixtures");
        let mut text = serde_json::to_string_pretty(&sorted(&Value::Object(merged))).unwrap();
        text.push('\n');
        fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        return;
    }
    let Some(existing) = existing else {
        if new.is_empty() {
            return;
        }
        panic!(
            "fixture file {} is missing; run `UPDATE_FIXTURES=1 cargo test -p dbstudio-core`",
            path.display()
        );
    };
    let mut drift = Vec::new();
    for (name, sample) in new {
        match existing.get(name) {
            None => drift.push(format!("{group}.{name}: missing from the fixture file")),
            Some(old) if old != sample => drift.push(format!(
                "{group}.{name}: fixture differs from the serialised sample\n  fixture: {old}\n  actual:  {sample}"
            )),
            Some(_) => {}
        }
    }
    assert!(
        drift.is_empty(),
        "contract drift (re-run with UPDATE_FIXTURES=1 after reviewing the TypeScript side):\n{}",
        drift.join("\n")
    );
}

/// Whether a missing integration-test service must fail the test instead of skipping it.
///
/// `DBSTUDIO_IT_STRICT` is `1` / `all` (every service is strict) or a comma-separated list of
/// service names as in `docker/compose.test.yml` (for example `scylla,cassandra5,sshd`); names are
/// compared case-insensitively. Unset, empty or `0` means nothing is strict.
pub fn it_strict_for(service: &str) -> bool {
    let Ok(value) = std::env::var("DBSTUDIO_IT_STRICT") else {
        return false;
    };
    let value = value.trim();
    if value == "1" || value.eq_ignore_ascii_case("all") {
        return true;
    }
    value
        .split(',')
        .any(|name| !name.trim().is_empty() && name.trim().eq_ignore_ascii_case(service))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorted_orders_keys_recursively() {
        let v = json!({"b": {"z": 1, "a": [{"y": 1, "x": 2}]}, "a": 0});
        let text = serde_json::to_string(&sorted(&v)).unwrap();
        assert_eq!(text, r#"{"a":0,"b":{"a":[{"x":2,"y":1}],"z":1}}"#);
    }
}
