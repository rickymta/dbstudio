//! Conversion of the shell AST (`dbstudio_sql::mongo_shell`) into BSON, and literal parsing
//! helpers for filters typed by the user.

use base64::Engine as _;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, Document};
use dbstudio_core::{Error, Result};
use dbstudio_sql::mongo_shell::{self, ShellValue};

fn bad(msg: impl Into<String>) -> Error {
    Error::Query {
        message: msg.into(),
        position: None,
    }
}

/// Parse the text of a shell `Date` literal (`2024-05-01`, RFC 3339, epoch millis, empty = now).
pub fn parse_shell_date(text: &str) -> Result<bson::DateTime> {
    let t = text.trim().trim_matches(|c| c == '"' || c == '\'');
    if t.is_empty() {
        return Ok(bson::DateTime::now());
    }
    if let Ok(ms) = t.parse::<i64>() {
        return Ok(bson::DateTime::from_millis(ms));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Ok(bson::DateTime::from_millis(dt.timestamp_millis()));
    }
    let utc =
        |n: chrono::NaiveDateTime| bson::DateTime::from_millis(n.and_utc().timestamp_millis());
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(t, fmt) {
            return Ok(utc(n));
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        if let Some(n) = d.and_hms_opt(0, 0, 0) {
            return Ok(utc(n));
        }
    }
    Err(bad(format!("invalid date '{t}'")))
}

/// Shell value -> BSON. Integer literals become Int32 when they fit, like `mongosh`.
pub fn shell_to_bson(v: &ShellValue) -> Result<Bson> {
    Ok(match v {
        ShellValue::Null => Bson::Null,
        ShellValue::Bool(b) => Bson::Boolean(*b),
        ShellValue::Int(i) => match i32::try_from(*i) {
            Ok(i) => Bson::Int32(i),
            Err(_) => Bson::Int64(*i),
        },
        ShellValue::Double(d) => Bson::Double(*d),
        ShellValue::String(s) => Bson::String(s.clone()),
        ShellValue::Array(a) => Bson::Array(a.iter().map(shell_to_bson).collect::<Result<_>>()?),
        ShellValue::Object(fields) => {
            let mut d = Document::new();
            for (k, v) in fields {
                d.insert(k.clone(), shell_to_bson(v)?);
            }
            Bson::Document(d)
        }
        ShellValue::ObjectId(h) => Bson::ObjectId(
            bson::oid::ObjectId::parse_str(h).map_err(|e| bad(format!("invalid ObjectId: {e}")))?,
        ),
        ShellValue::Date(t) => Bson::DateTime(parse_shell_date(t)?),
        ShellValue::NumberInt(i) => Bson::Int32(*i),
        ShellValue::NumberLong(i) => Bson::Int64(*i),
        ShellValue::NumberDecimal(s) => Bson::Decimal128(
            s.parse::<bson::Decimal128>()
                .map_err(|e| bad(format!("invalid Decimal128: {e}")))?,
        ),
        ShellValue::Uuid(s) => {
            let u = uuid::Uuid::parse_str(s).map_err(|e| bad(format!("invalid UUID: {e}")))?;
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: u.as_bytes().to_vec(),
            })
        }
        ShellValue::BinData { subtype, base64 } => Bson::Binary(Binary {
            subtype: BinarySubtype::from(*subtype),
            bytes: base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|e| bad(format!("invalid base64: {e}")))?,
        }),
        ShellValue::Timestamp { t, i } => Bson::Timestamp(bson::Timestamp {
            time: *t,
            increment: *i,
        }),
        ShellValue::MinKey => Bson::MinKey,
        ShellValue::MaxKey => Bson::MaxKey,
        ShellValue::Regex { pattern, flags } => Bson::RegularExpression(bson::Regex {
            pattern: pattern
                .clone()
                .try_into()
                .map_err(|_| bad("invalid regex"))?,
            options: flags
                .clone()
                .try_into()
                .map_err(|_| bad("invalid regex flags"))?,
        }),
    })
}

/// Shell value that must be a document.
pub fn shell_to_doc(v: &ShellValue) -> Result<Document> {
    match shell_to_bson(v)? {
        Bson::Document(d) => Ok(d),
        other => Err(bad(format!(
            "expected a document, got {}",
            other.element_type() as u8
        ))),
    }
}

/// Parse a user literal: EJSON/JSON first, then shell literals (`ObjectId("..")`, `ISODate(..)`),
/// otherwise `None`.
pub fn parse_literal(text: &str) -> Option<Bson> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(b) = serde_json::from_str::<Bson>(t) {
        return Some(b);
    }
    // Shell syntax through the shared parser: `db.x.find(<literal>)`.
    let stmt = format!("db.x.find({t})");
    let parsed = mongo_shell::parse(&stmt).ok()?;
    let args = &parsed.first()?.args;
    shell_to_bson(args.first()?).ok()
}

/// A filter document typed by the user: EJSON relaxed/canonical or a shell object literal.
pub fn parse_filter_text(text: &str) -> Result<Document> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(Document::new());
    }
    match crate::convert::parse_ejson_doc(t) {
        Ok(d) => Ok(d),
        Err(json_err) => {
            let stmt = format!("db.x.find({t})");
            match mongo_shell::parse(&stmt) {
                Ok(p) => match p.first().and_then(|s| s.args.first()) {
                    Some(v) => shell_to_doc(v),
                    None => Err(json_err),
                },
                Err(_) => Err(json_err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_values_convert() {
        let v = ShellValue::Object(vec![
            ("a".into(), ShellValue::Int(1)),
            ("big".into(), ShellValue::Int(1 << 40)),
            (
                "o".into(),
                ShellValue::ObjectId("65f0a1b2c3d4e5f607182930".into()),
            ),
            ("d".into(), ShellValue::Date("2024-05-01".into())),
            ("l".into(), ShellValue::NumberLong(5)),
            ("n".into(), ShellValue::NumberDecimal("1.50".into())),
        ]);
        let d = shell_to_doc(&v).unwrap();
        assert_eq!(d.get("a"), Some(&Bson::Int32(1)));
        assert_eq!(d.get("big"), Some(&Bson::Int64(1 << 40)));
        assert!(matches!(d.get("o"), Some(Bson::ObjectId(_))));
        assert_eq!(
            d.get("d"),
            Some(&Bson::DateTime(bson::DateTime::from_millis(
                1_714_521_600_000
            )))
        );
        assert_eq!(d.get("l"), Some(&Bson::Int64(5)));
        assert!(matches!(d.get("n"), Some(Bson::Decimal128(_))));
        let keys: Vec<_> = d.keys().cloned().collect();
        assert_eq!(keys, ["a", "big", "o", "d", "l", "n"]);
    }

    #[test]
    fn literals_prefer_json() {
        assert_eq!(parse_literal("123"), Some(Bson::Int32(123)));
        assert_eq!(parse_literal("true"), Some(Bson::Boolean(true)));
        assert_eq!(parse_literal("\"x\""), Some(Bson::String("x".into())));
        assert!(matches!(
            parse_literal("{\"$oid\":\"65f0a1b2c3d4e5f607182930\"}"),
            Some(Bson::ObjectId(_))
        ));
        assert_eq!(parse_literal("plain text"), None);
    }

    #[test]
    fn filter_text_accepts_ejson() {
        let d = parse_filter_text("{\"age\": {\"$gt\": 30}}").unwrap();
        assert!(d.get_document("age").is_ok());
        assert!(parse_filter_text("{oops").is_err());
        assert!(parse_filter_text("  ").unwrap().is_empty());
    }
}
