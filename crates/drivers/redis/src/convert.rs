//! Conversions between client values and [`dbstudio_core::Value`].

use dbstudio_core::{Error, Result, Value};

/// UTF-8 bytes become `Text`, anything else `Bytes`.
pub fn bytes_value(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(s) => Value::Text(s),
        Err(e) => Value::Bytes(e.into_bytes()),
    }
}

/// Raw bytes of a value typed by the user (member, field, string value...).
pub fn value_bytes(v: &Value) -> Result<Vec<u8>> {
    Ok(match v {
        Value::Text(s) | Value::Decimal(s) | Value::Json(s) => s.as_bytes().to_vec(),
        Value::Bytes(b) => b.clone(),
        Value::Int(i) => i.to_string().into_bytes(),
        Value::UInt(i) => i.to_string().into_bytes(),
        Value::Float(f) => f.to_string().into_bytes(),
        Value::Bool(b) => (if *b { "1" } else { "0" }).as_bytes().to_vec(),
        other => {
            return Err(Error::Other(format!(
                "value cannot be stored in Redis: {other:?}"
            )));
        }
    })
}

/// Convert a server reply (recursively) to a [`Value`].
pub fn reply_value(v: redis::Value) -> Value {
    use redis::Value as R;
    match v {
        R::Nil => Value::Null,
        R::Int(i) => Value::Int(i),
        R::BulkString(b) => bytes_value(b),
        R::SimpleString(s) => Value::Text(s),
        R::Okay => Value::Text("OK".into()),
        R::Double(f) => Value::Float(f),
        R::Boolean(b) => Value::Bool(b),
        R::VerbatimString { text, .. } => Value::Text(text),
        R::BigNumber(n) => Value::Decimal(format!("{n:?}")),
        R::Array(items) | R::Set(items) => {
            Value::Array(items.into_iter().map(reply_value).collect())
        }
        R::Map(pairs) => Value::Document(
            pairs
                .into_iter()
                .map(|(k, v)| (reply_text(&k), reply_value(v)))
                .collect(),
        ),
        R::Attribute { data, .. } => reply_value(*data),
        R::Push { data, .. } => Value::Array(data.into_iter().map(reply_value).collect()),
        R::ServerError(e) => Value::Text(format!("(error) {e:?}")),
        other => Value::Text(format!("{other:?}")),
    }
}

/// Text of a scalar reply (used for map keys and INFO fields).
pub fn reply_text(v: &redis::Value) -> String {
    use redis::Value as R;
    match v {
        R::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        R::SimpleString(s) => s.clone(),
        R::Int(i) => i.to_string(),
        R::Okay => "OK".into(),
        R::Double(f) => f.to_string(),
        R::VerbatimString { text, .. } => text.clone(),
        _ => String::new(),
    }
}

/// Raw bytes of a scalar reply.
pub fn reply_bytes(v: &redis::Value) -> Option<Vec<u8>> {
    use redis::Value as R;
    match v {
        R::BulkString(b) => Some(b.clone()),
        R::SimpleString(s) => Some(s.as_bytes().to_vec()),
        R::Int(i) => Some(i.to_string().into_bytes()),
        _ => None,
    }
}

/// Cut a prefix read with `GETRANGE` at a character boundary when only the last (partial)
/// UTF-8 sequence is invalid.
pub fn trim_partial_utf8(mut bytes: Vec<u8>) -> Vec<u8> {
    if let Err(e) = std::str::from_utf8(&bytes)
        && e.error_len().is_none()
    {
        bytes.truncate(e.valid_up_to());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_is_text_and_binary_is_bytes() {
        assert_eq!(bytes_value(b"abc".to_vec()), Value::Text("abc".into()));
        assert_eq!(bytes_value(vec![0xff]), Value::Bytes(vec![0xff]));
    }

    #[test]
    fn partial_trailing_sequence_is_trimmed() {
        let mut b = "aé".as_bytes().to_vec();
        b.pop();
        assert_eq!(trim_partial_utf8(b), b"a".to_vec());
        assert_eq!(trim_partial_utf8(vec![0xff, b'a']), vec![0xff, b'a']);
    }

    #[test]
    fn replies_convert_recursively() {
        let v = reply_value(redis::Value::Array(vec![
            redis::Value::Int(1),
            redis::Value::Nil,
            redis::Value::BulkString(b"x".to_vec()),
        ]));
        assert_eq!(
            v,
            Value::Array(vec![Value::Int(1), Value::Null, Value::Text("x".into())])
        );
    }

    #[test]
    fn value_bytes_accepts_scalars_only() {
        assert_eq!(value_bytes(&Value::Int(5)).unwrap(), b"5");
        assert!(value_bytes(&Value::Array(vec![])).is_err());
    }
}
