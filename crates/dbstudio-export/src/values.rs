//! Rendering of cell values for the text formats (EXPORT-08).

use std::borrow::Cow;
use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use dbstudio_core::Value;

use crate::{BinaryFormat, ExtendedJson};

/// Largest integer that survives a round trip through a JavaScript number.
const JS_MAX_SAFE: i64 = 9_007_199_254_740_991;

/// Binary data as base64 or lowercase hex text.
pub(crate) fn encode_binary(bytes: &[u8], format: BinaryFormat) -> String {
    match format {
        BinaryFormat::Base64 => STANDARD.encode(bytes),
        BinaryFormat::Hex => {
            let mut s = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                let _ = write!(s, "{b:02x}");
            }
            s
        }
    }
}

/// Plain text of a cell for CSV / XLSX text cells; `None` for NULL.
pub(crate) fn cell_text(v: &Value, binary: BinaryFormat) -> Option<Cow<'_, str>> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        Value::Int(i) => Cow::Owned(i.to_string()),
        Value::UInt(u) => Cow::Owned(u.to_string()),
        Value::Float(f) => Cow::Owned(float_text(*f)),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s)
        | Value::Json(s) => Cow::Borrowed(s.as_str()),
        Value::Bytes(b) => Cow::Owned(encode_binary(b, binary)),
        Value::Ext(e) => Cow::Borrowed(e.text.as_str()),
        Value::Array(_) | Value::Document(_) => {
            let mut s = String::new();
            write_json(&mut s, v, binary, ExtendedJson::Off, None, 0);
            Cow::Owned(s)
        }
    })
}

fn float_text(f: f64) -> String {
    if f.is_finite() {
        format!("{f:?}")
    } else if f.is_nan() {
        "NaN".into()
    } else if f.is_sign_positive() {
        "Infinity".into()
    } else {
        "-Infinity".into()
    }
}

/// JSON string literal.
pub(crate) fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(step) = indent {
        out.push('\n');
        for _ in 0..level * step {
            out.push(' ');
        }
    }
}

/// Append `v` as JSON. `indent` = spaces per level (pretty printing) starting at `level`.
/// Field order of documents is kept; big integers and decimals become strings.
pub(crate) fn write_json(
    out: &mut String,
    v: &Value,
    binary: BinaryFormat,
    ext: ExtendedJson,
    indent: Option<usize>,
    level: usize,
) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            if (-JS_MAX_SAFE..=JS_MAX_SAFE).contains(i) {
                let _ = write!(out, "{i}");
            } else {
                json_string(out, &i.to_string());
            }
        }
        Value::UInt(u) => {
            if *u <= JS_MAX_SAFE as u64 {
                let _ = write!(out, "{u}");
            } else {
                json_string(out, &u.to_string());
            }
        }
        Value::Float(f) => {
            if f.is_finite() {
                let _ = write!(out, "{f:?}");
            } else {
                json_string(out, &float_text(*f));
            }
        }
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::Timestamp(s)
        | Value::Uuid(s) => json_string(out, s),
        Value::Json(s) => {
            // Embed valid JSON as is (trimmed so a trailing newline does not leak into the row).
            if serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok() {
                out.push_str(s.trim());
            } else {
                json_string(out, s);
            }
        }
        Value::Bytes(b) => json_string(out, &encode_binary(b, binary)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                write_json(out, item, binary, ext, indent, level + 1);
            }
            newline(out, indent, level);
            out.push(']');
        }
        Value::Document(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                json_string(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_json(out, item, binary, ext, indent, level + 1);
            }
            newline(out, indent, level);
            out.push('}');
        }
        Value::Ext(e) => write_ext(out, &e.ty, &e.text, ext),
    }
}

/// Database specific value: plain text, or an Extended JSON wrapper when requested.
fn write_ext(out: &mut String, ty: &str, text: &str, mode: ExtendedJson) {
    let wrapper = match (mode, ty) {
        (ExtendedJson::Off, _) => None,
        (_, "objectId") => Some("$oid"),
        (_, "decimal128" | "decimal") => Some("$numberDecimal"),
        (ExtendedJson::Canonical, "int64" | "long") => Some("$numberLong"),
        (_, "date") => Some("$date"),
        _ => None,
    };
    match (mode, ty, wrapper) {
        (_, _, Some(key)) => {
            out.push('{');
            json_string(out, key);
            out.push(':');
            json_string(out, text);
            out.push('}');
        }
        // Relaxed EJSON writes 64-bit integers as plain numbers.
        (ExtendedJson::Relaxed, "int64" | "long", None) if text.parse::<i64>().is_ok() => {
            out.push_str(text);
        }
        _ => json_string(out, text),
    }
}

/// How JSON values are rendered.
#[derive(Debug, Clone, Copy)]
pub(crate) struct JsonStyle {
    pub binary: BinaryFormat,
    pub ext: ExtendedJson,
    /// Spaces per level when pretty printing.
    pub indent: Option<usize>,
}

/// One row as a JSON object with the given keys.
pub(crate) fn write_json_row(
    out: &mut String,
    names: &[String],
    keep: &[usize],
    row: &[Value],
    style: JsonStyle,
    level: usize,
) {
    if keep.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    for (n, &i) in keep.iter().enumerate() {
        if n > 0 {
            out.push(',');
        }
        newline(out, style.indent, level + 1);
        json_string(out, &names[i]);
        out.push(':');
        if style.indent.is_some() {
            out.push(' ');
        }
        match row.get(i) {
            Some(v) => write_json(out, v, style.binary, style.ext, style.indent, level + 1),
            None => out.push_str("null"),
        }
    }
    newline(out, style.indent, level);
    out.push('}');
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::ExtValue;

    fn json(v: &Value) -> String {
        let mut s = String::new();
        write_json(&mut s, v, BinaryFormat::Base64, ExtendedJson::Off, None, 0);
        s
    }

    #[test]
    fn scalars() {
        assert_eq!(json(&Value::Null), "null");
        assert_eq!(json(&Value::Int(-5)), "-5");
        assert_eq!(
            json(&Value::Int(9_007_199_254_740_993)),
            "\"9007199254740993\""
        );
        assert_eq!(json(&Value::Decimal("1.50".into())), "\"1.50\"");
        assert_eq!(json(&Value::Float(1.5)), "1.5");
        assert_eq!(json(&Value::Float(f64::NAN)), "\"NaN\"");
        assert_eq!(json(&Value::Text("a\"b\n".into())), "\"a\\\"b\\n\"");
        assert_eq!(json(&Value::Bytes(vec![1, 2, 3])), "\"AQID\"");
        assert_eq!(json(&Value::Json("{\"a\": 1}".into())), "{\"a\": 1}");
        assert_eq!(json(&Value::Json("not json".into())), "\"not json\"");
    }

    #[test]
    fn nested_and_pretty() {
        let v = Value::Document(vec![
            ("b".into(), Value::Int(1)),
            (
                "a".into(),
                Value::Array(vec![Value::Bool(true), Value::Null]),
            ),
        ]);
        assert_eq!(json(&v), "{\"b\":1,\"a\":[true,null]}");
        let mut s = String::new();
        write_json(&mut s, &v, BinaryFormat::Hex, ExtendedJson::Off, Some(2), 0);
        assert_eq!(s, "{\n  \"b\": 1,\n  \"a\": [\n    true,\n    null\n  ]\n}");
    }

    #[test]
    fn binary_formats_and_cell_text() {
        assert_eq!(encode_binary(&[0xde, 0xad], BinaryFormat::Hex), "dead");
        assert_eq!(cell_text(&Value::Null, BinaryFormat::Hex), None);
        assert_eq!(
            cell_text(&Value::Bytes(vec![255]), BinaryFormat::Hex).unwrap(),
            "ff"
        );
        assert_eq!(
            cell_text(&Value::Bool(false), BinaryFormat::Hex).unwrap(),
            "false"
        );
        assert_eq!(
            cell_text(
                &Value::Array(vec![Value::Int(1), Value::Int(2)]),
                BinaryFormat::Hex
            )
            .unwrap(),
            "[1,2]"
        );
    }

    #[test]
    fn extended_json() {
        let oid = Value::Ext(ExtValue::new("objectId", "507f1f77bcf86cd799439011"));
        let mut s = String::new();
        write_json(
            &mut s,
            &oid,
            BinaryFormat::Hex,
            ExtendedJson::Relaxed,
            None,
            0,
        );
        assert_eq!(s, "{\"$oid\":\"507f1f77bcf86cd799439011\"}");
        let long = Value::Ext(ExtValue::new("int64", "5"));
        let mut r = String::new();
        write_json(
            &mut r,
            &long,
            BinaryFormat::Hex,
            ExtendedJson::Relaxed,
            None,
            0,
        );
        assert_eq!(r, "5");
        let mut c = String::new();
        write_json(
            &mut c,
            &long,
            BinaryFormat::Hex,
            ExtendedJson::Canonical,
            None,
            0,
        );
        assert_eq!(c, "{\"$numberLong\":\"5\"}");
        assert_eq!(json(&oid), "\"507f1f77bcf86cd799439011\"");
    }
}
