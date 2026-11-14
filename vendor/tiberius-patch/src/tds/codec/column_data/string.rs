use std::borrow::Cow;

use crate::{error::Error, sql_read_bytes::SqlReadBytes, tds::Collation, VarLenType};

pub(crate) async fn decode<R>(
    src: &mut R,
    ty: VarLenType,
    len: usize,
    collation: Option<Collation>,
) -> crate::Result<Option<Cow<'static, str>>>
where
    R: SqlReadBytes + Unpin,
{
    use VarLenType::*;

    // DBStudio patch: honour the per-request PLP byte limit.
    let limit = src.context().plp_limit();
    let (mut data, truncated) = super::plp::decode_limited(src, len, limit).await?;
    if let Some(total) = truncated {
        src.context_mut().note_plp_truncated(total);
        // The cut may split a character: keep whole UTF-16 code units; code pages trim below.
        if let Some(buf) = data.as_mut() {
            if !matches!(ty, BigChar | BigVarChar) {
                buf.truncate(buf.len() & !1);
                if buf.len() >= 2 {
                    let last = u16::from_le_bytes([buf[buf.len() - 2], buf[buf.len() - 1]]);
                    if (0xD800..0xDC00).contains(&last) {
                        buf.truncate(buf.len() - 2);
                    }
                }
            }
        }
    }

    match (data, ty) {
        // Codepages other than UTF
        (Some(buf), BigChar) | (Some(buf), BigVarChar) => {
            let collation = collation
                .as_ref()
                .ok_or_else(|| Error::Protocol("string column missing collation".into()))?;
            let encoder = collation.encoding()?;

            let s = if truncated.is_some() {
                // DBStudio patch: a cut may end inside a multi-byte character; drop the dangling
                // tail bytes (at most 3) and decode the rest strictly.
                let mut keep = buf.len();
                loop {
                    if let Some(s) = encoder
                        .decode_without_bom_handling_and_without_replacement(&buf[..keep])
                    {
                        break s.to_string();
                    }
                    if keep == 0 || buf.len() - keep >= 3 {
                        return Err(Error::Encoding("invalid sequence".into()));
                    }
                    keep -= 1;
                }
            } else {
                encoder
                    .decode_without_bom_handling_and_without_replacement(buf.as_ref())
                    .ok_or_else(|| Error::Encoding("invalid sequence".into()))?
                    .to_string()
            };

            Ok(Some(s.into()))
        }
        // UTF-16
        (Some(buf), _) => {
            if buf.len() % 2 != 0 {
                return Err(Error::Protocol("nvarchar: invalid plp length".into()));
            }

            // Decode UTF-16LE straight from the byte pairs, without first
            // collecting an intermediate `Vec<u16>` (one fewer full-buffer
            // allocation + copy per value).
            let units = buf.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]]));

            // NVARCHAR/NCHAR may be decoded losslessly when the connection opted
            // in (`Config::lossy_utf16_decoding`), replacing invalid surrogates
            // with U+FFFD so legacy rows holding unchecked UCS-2 stay readable.
            // XML (`ty == Xml`, routed here from `xml::decode`) is always strict,
            // regardless of the flag. Either way the `buf.len() % 2` guard above
            // already rejected desynced (odd) lengths.
            if matches!(ty, NChar | NVarchar) && src.context().lossy_utf16() {
                let s: String = char::decode_utf16(units)
                    .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect();
                Ok(Some(s.into()))
            } else {
                // Strict: invalid surrogates error, matching the previous
                // `String::from_utf16` behaviour.
                let s = char::decode_utf16(units)
                    .collect::<Result<String, _>>()
                    .map_err(|_| Error::Protocol("nvarchar: invalid UTF-16 sequence".into()))?;
                Ok(Some(s.into()))
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_read_bytes::test_utils::IntoSqlReadBytes;
    use bytes::{BufMut, BytesMut};

    // Lossy NVARCHAR decoding replaces an unpaired surrogate with U+FFFD.
    #[tokio::test]
    async fn nvarchar_lossy_replaces_lone_surrogate() {
        let mut buf = BytesMut::new();
        buf.put_u16_le(2); // fixed-size PLP length prefix
        buf.put_u16_le(0xD800); // unpaired high surrogate

        let mut reader = buf.into_sql_read_bytes();
        reader.context_mut().set_lossy_utf16(true);
        let value = decode(&mut reader, VarLenType::NVarchar, 40, None)
            .await
            .expect("lossy nvarchar must decode malformed UTF-16");
        assert_eq!(value.as_deref(), Some("\u{fffd}"));
    }

    // Strict is the default: an unpaired surrogate is a protocol error.
    #[tokio::test]
    async fn nvarchar_strict_rejects_lone_surrogate() {
        let mut buf = BytesMut::new();
        buf.put_u16_le(2);
        buf.put_u16_le(0xD800);

        let err = decode(
            &mut buf.into_sql_read_bytes(),
            VarLenType::NVarchar,
            40,
            None,
        )
        .await
        .expect_err("strict nvarchar must reject malformed UTF-16");
        assert!(matches!(err, Error::Protocol(_)));
    }

    // A BigVarChar (non-UTF codepage) value with the collation omitted by the
    // server must return a protocol error rather than panicking on `unwrap`.
    #[tokio::test]
    async fn decode_bigvarchar_missing_collation_errors() {
        let mut buf = BytesMut::new();
        buf.put_u16_le(2); // fixed-size PLP length prefix
        buf.put_slice(&[0x41, 0x42]); // "AB" in an 8-bit codepage

        let err = decode(
            &mut buf.into_sql_read_bytes(),
            VarLenType::BigVarChar,
            2,
            None,
        )
        .await
        .expect_err("missing collation must error, not panic");

        match err {
            Error::Protocol(msg) => {
                assert!(
                    msg.contains("missing collation"),
                    "unexpected protocol message: {msg}"
                );
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    // DBStudio patch: a limit that lands between the halves of a surrogate pair drops the
    // dangling high surrogate instead of failing the row.
    #[tokio::test]
    async fn limited_nvarchar_drops_a_dangling_high_surrogate() {
        let mut buf = BytesMut::new();
        buf.put_u64_le(0xffff_ffff_ffff_fffe);
        let units: [u16; 3] = [0x41, 0xD83D, 0xDE00]; // "A" + U+1F600
        buf.put_u32_le(6);
        for u in units {
            buf.put_u16_le(u);
        }
        buf.put_u32_le(0);
        let mut reader = buf.into_sql_read_bytes();
        reader.context_mut().set_plp_limit(Some(4)); // "A" + the high surrogate
        let v = decode(&mut reader, VarLenType::NVarchar, 0xffff_ffff, None)
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some("A"));
        assert_eq!(reader.context_mut().take_plp_truncated(), Some(6));
    }

    // An odd limit never leaves half a UTF-16 unit.
    #[tokio::test]
    async fn limited_nvarchar_with_odd_limit_keeps_whole_units() {
        let mut buf = BytesMut::new();
        buf.put_u64_le(0xffff_ffff_ffff_fffe);
        buf.put_u32_le(6);
        for u in [0x41u16, 0x42, 0x43] {
            buf.put_u16_le(u);
        }
        buf.put_u32_le(0);
        let mut reader = buf.into_sql_read_bytes();
        reader.context_mut().set_plp_limit(Some(3));
        let v = decode(&mut reader, VarLenType::NVarchar, 0xffff_ffff, None)
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some("A"));
    }
}
