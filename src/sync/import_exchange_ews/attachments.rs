/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::base64::LENIENT;

use crate::db::blobs;
use crate::error::Error;
use crate::exchange_ews::error::EwsError;
use crate::exchange_ews::parse::parse_get_attachment_inline;
use crate::exchange_ews::xml::get_attachment_body;

use super::items::ItemRunCtx;

#[derive(Debug, Clone)]
pub struct FetchedAttachment {
    pub attachment_id: String,
    pub bytes: Vec<u8>,
    pub media_type: String,
}

pub fn fetch_contact_photo(
    conn: &rusqlite::Connection,
    ctx: &ItemRunCtx<'_>,
    attachment_id: &str,
) -> Result<Option<(i64, String)>, Error> {
    let Some(att) = fetch_attachment_bytes(ctx, attachment_id).map_err(Error::from)? else {
        return Ok(None);
    };
    let blob_id = intern_attachment(conn, &att.bytes)?;
    Ok(Some((blob_id, att.media_type)))
}

pub fn fetch_attachment_bytes(
    ctx: &ItemRunCtx<'_>,
    attachment_id: &str,
) -> Result<Option<FetchedAttachment>, EwsError> {
    let mut out = fetch_attachments(ctx, &[attachment_id])?;
    Ok(out.pop())
}

pub fn fetch_attachments(
    ctx: &ItemRunCtx<'_>,
    attachment_ids: &[&str],
) -> Result<Vec<FetchedAttachment>, EwsError> {
    if attachment_ids.is_empty() {
        return Ok(Vec::new());
    }
    let batch = ctx.attachment_batch.max(1);
    let mut out: Vec<FetchedAttachment> = Vec::with_capacity(attachment_ids.len());
    for chunk in attachment_ids.chunks(batch) {
        let body = get_attachment_body(chunk);
        let resp = ctx.client.call(ctx.url, "GetAttachment", &body)?;
        let inline = parse_get_attachment_inline(&resp.body)?;
        for att in inline {
            let bytes = LENIENT.decode(&att.content_base64).map_err(|e| {
                EwsError::Malformed(format!("attachment {}: base64: {e}", att.attachment_id))
            })?;
            if bytes.is_empty() {
                continue;
            }
            let media_type = att
                .content_type
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            out.push(FetchedAttachment {
                attachment_id: att.attachment_id,
                bytes,
                media_type,
            });
        }
    }
    Ok(out)
}

pub fn intern_attachment(conn: &rusqlite::Connection, bytes: &[u8]) -> Result<i64, Error> {
    blobs::intern_blob(conn, bytes).map_err(|e| Error::Partial(e.to_string()))
}
