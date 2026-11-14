use std::borrow::Cow;

use crate::{sql_read_bytes::SqlReadBytes, ColumnData};

pub(crate) async fn decode<R>(src: &mut R, len: usize) -> crate::Result<ColumnData<'static>>
where
    R: SqlReadBytes + Unpin,
{
    // DBStudio patch: honour the per-request PLP byte limit.
    let limit = src.context().plp_limit();
    let (data, truncated) = super::plp::decode_limited(src, len, limit).await?;
    if let Some(total) = truncated {
        src.context_mut().note_plp_truncated(total);
    }
    let data = data.map(Cow::from);

    Ok(ColumnData::Binary(data))
}
