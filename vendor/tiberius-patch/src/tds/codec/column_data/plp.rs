use crate::sql_read_bytes::SqlReadBytes;

// Decode a partially length-prefixed type.
//
// NOTE: the length/chunk prefixes are read with the packet-aware
// `read_u16_le` (fixed-size total length), `read_u64_le` (PLP total-length
// field) and `read_u32_le` (per-chunk length) helpers, and the value bytes
// themselves via the packet-aware bulk reader `read_bytes_into`. All of these
// transparently span TDS packet boundaries. The generic
// `AsyncReadExt::read_exact` must NOT be used to read the value bytes here: a
// PLP value can span multiple packets, and `read_exact` treats a
// packet-boundary `Ok(0)` as EOF (truncating the value), whereas
// `read_bytes_into` continues across the boundary.
pub(crate) async fn decode<R>(src: &mut R, len: usize) -> crate::Result<Option<Vec<u8>>>
where
    R: SqlReadBytes + Unpin,
{
    Ok(decode_limited(src, len, None).await?.0)
}

// DBStudio patch: like `decode`, but keeps at most `limit` bytes of a PLP value. The rest is read
// off the wire and dropped in small windows, so memory stays bounded. The second tuple field is
// the full value size in bytes when the value was cut.
pub(crate) async fn decode_limited<R>(
    src: &mut R,
    len: usize,
    limit: Option<usize>,
) -> crate::Result<(Option<Vec<u8>>, Option<u64>)>
where
    R: SqlReadBytes + Unpin,
{
    match len {
        // Fixed size
        len if len < 0xffff => {
            let len = src.read_u16_le().await? as usize;

            match len {
                // NULL
                0xffff => Ok((None, None)),
                _ => {
                    let mut data = Vec::new();
                    crate::sql_read_bytes::read_bytes_into(
                        src,
                        &mut data,
                        len,
                        super::MAX_PREALLOC,
                    )
                    .await?;

                    Ok((Some(data), None))
                }
            }
        }
        // Unknown size, length-prefixed blobs
        _ => {
            let len = src.read_u64_le().await?;

            let mut data = match len {
                // NULL
                0xffffffffffffffff => return Ok((None, None)),
                // Unknown size
                0xfffffffffffffffe => Vec::new(),
                // Known size. `len` is an untrusted 64-bit wire value; cap the
                // up-front reservation (avoids memory-exhaustion and the
                // `Vec` capacity-overflow panic for values near u64::MAX).
                _ => Vec::with_capacity((len as usize).min(super::MAX_PREALLOC)),
            };
            // DBStudio patch: the effective cap never exceeds the global PLP size limit.
            let limit = limit.map(|l| l.min(super::MAX_PLP_SIZE));
            let cap = limit.unwrap_or(usize::MAX);
            let mut total: u64 = 0;
            let mut scratch: Vec<u8> = Vec::new();

            loop {
                let chunk_size = src.read_u32_le().await? as usize;

                if chunk_size == 0 {
                    break; // found a sentinel, we're done
                }

                // The number of chunks in an "unknown length" PLP value is
                // unbounded on the wire. Cap the running total so a hostile
                // server cannot stream chunks forever and exhaust memory on
                // a single value.
                // DBStudio patch: past the caller's limit, keep what fits and skip the rest.
                if limit.is_some() {
                    let keep = cap.saturating_sub(data.len()).min(chunk_size);
                    if keep > 0 {
                        crate::sql_read_bytes::read_bytes_into(
                            src,
                            &mut data,
                            keep,
                            super::MAX_PREALLOC,
                        )
                        .await?;
                    }
                    let mut skip = chunk_size - keep;
                    while skip > 0 {
                        let step = skip.min(64 * 1024);
                        scratch.clear();
                        crate::sql_read_bytes::read_bytes_into(src, &mut scratch, step, step)
                            .await?;
                        skip -= step;
                    }
                    total = total.saturating_add(chunk_size as u64);
                    continue;
                }

                if data.len().saturating_add(chunk_size) > super::MAX_PLP_SIZE {
                    return Err(crate::Error::Protocol(
                        format!(
                            "PLP value exceeds the maximum supported size of {} bytes",
                            super::MAX_PLP_SIZE
                        )
                        .into(),
                    ));
                }

                // Bulk-read the whole chunk (packet-aware), appending it to the
                // running accumulation buffer.
                crate::sql_read_bytes::read_bytes_into(
                    src,
                    &mut data,
                    chunk_size,
                    super::MAX_PREALLOC,
                )
                .await?;
            }

            let truncated = (limit.is_some() && total > data.len() as u64).then_some(total);
            Ok((Some(data), truncated))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_read_bytes::test_utils::IntoSqlReadBytes;
    use bytes::{BufMut, BytesMut};

    // At the boundary `len == 0xffff` the decoder takes the chunked (PLP) branch,
    // reading a `u64` length; the chunked stream decodes to [0xAA, 0xBB, 0xCC].
    #[tokio::test]
    async fn decode_boundary_len_uses_chunked_branch() {
        let mut buf = BytesMut::new();
        buf.put_u64_le(5); // known-size PLP total length
        buf.put_u32_le(3); // chunk size
        buf.put_slice(&[0xAA, 0xBB, 0xCC]);
        buf.put_u32_le(0); // terminating chunk

        let data = decode(&mut buf.into_sql_read_bytes(), 0xffff)
            .await
            .unwrap();
        assert_eq!(data, Some(vec![0xAA, 0xBB, 0xCC]));
    }

    // A chunk whose running total strictly exceeds `MAX_PLP_SIZE` must be
    // rejected with the "exceeds the maximum" protocol error before any chunk
    // data is read.
    #[tokio::test]
    async fn decode_oversized_chunk_is_rejected() {
        let mut buf = BytesMut::new();
        buf.put_u64_le(10); // known-size marker -> chunked branch bookkeeping
        buf.put_u32_le(u32::MAX); // chunk size well past MAX_PLP_SIZE

        let err = decode(&mut buf.into_sql_read_bytes(), 0x10000)
            .await
            .expect_err("oversized PLP chunk must be rejected");

        match err {
            crate::Error::Protocol(msg) => {
                assert!(
                    msg.contains("exceeds the maximum"),
                    "unexpected protocol message: {msg}"
                );
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }
}

// DBStudio patch: tests of the byte-limited PLP read.
#[cfg(test)]
mod limit_tests {
    use super::*;
    use crate::sql_read_bytes::test_utils::IntoSqlReadBytes;
    use bytes::{BufMut, BytesMut};

    fn plp(total: u64, chunks: &[&[u8]]) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u64_le(total);
        for c in chunks {
            buf.put_u32_le(c.len() as u32);
            buf.extend_from_slice(c);
        }
        buf.put_u32_le(0);
        buf.put_u8(0x7f); // the next field must stay readable
        buf
    }

    const UNKNOWN: u64 = 0xffff_ffff_ffff_fffe;

    #[tokio::test]
    async fn cut_inside_a_chunk_reports_total_and_stays_in_sync() {
        let mut r = plp(UNKNOWN, &[b"abcdefgh"]).into_sql_read_bytes();
        let (data, cut) = decode_limited(&mut r, 0xffff_ffff, Some(3)).await.unwrap();
        assert_eq!(data.as_deref(), Some(&b"abc"[..]));
        assert_eq!(cut, Some(8));
        assert_eq!(r.read_u8().await.unwrap(), 0x7f);
    }

    #[tokio::test]
    async fn cut_across_many_chunks_known_length() {
        let mut r = plp(10, &[b"abcd", b"efgh", b"ij"]).into_sql_read_bytes();
        let (data, cut) = decode_limited(&mut r, 0xffff_ffff, Some(6)).await.unwrap();
        assert_eq!(data.as_deref(), Some(&b"abcdef"[..]));
        assert_eq!(cut, Some(10));
        assert_eq!(r.read_u8().await.unwrap(), 0x7f);
    }

    #[tokio::test]
    async fn value_at_or_under_the_limit_is_whole() {
        let mut r = plp(UNKNOWN, &[b"abcd"]).into_sql_read_bytes();
        let (data, cut) = decode_limited(&mut r, 0xffff_ffff, Some(4)).await.unwrap();
        assert_eq!(data.as_deref(), Some(&b"abcd"[..]));
        assert_eq!(cut, None);
    }

    #[tokio::test]
    async fn no_limit_reads_everything_and_null_stays_null() {
        let mut r = plp(UNKNOWN, &[b"abcd", b"ef"]).into_sql_read_bytes();
        assert_eq!(
            decode(&mut r, 0xffff_ffff).await.unwrap().as_deref(),
            Some(&b"abcdef"[..])
        );
        let mut buf = BytesMut::new();
        buf.put_u64_le(0xffff_ffff_ffff_ffff);
        let mut r = buf.into_sql_read_bytes();
        let (data, cut) = decode_limited(&mut r, 0xffff_ffff, Some(1)).await.unwrap();
        assert!(data.is_none() && cut.is_none());
    }

    #[tokio::test]
    async fn skipped_tail_larger_than_a_window_is_dropped() {
        let big = vec![7u8; 200_000];
        let mut r = plp(UNKNOWN, &[&big, b"zz"]).into_sql_read_bytes();
        let (data, cut) = decode_limited(&mut r, 0xffff_ffff, Some(2)).await.unwrap();
        assert_eq!(data.as_deref(), Some(&[7u8, 7][..]));
        assert_eq!(cut, Some(200_002));
        assert_eq!(r.read_u8().await.unwrap(), 0x7f);
    }
}
