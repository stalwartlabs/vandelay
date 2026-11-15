/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::base64::LENIENT;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::GetAttachmentInline;
use crate::exchange::ews::xml::EwsRequest;

use super::items::ItemRunCtx;

const FALLBACK_MEDIA_TYPE: &str = "application/octet-stream";

#[derive(Debug, Clone)]
pub struct FetchedAttachment {
    pub attachment_id: String,
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl ItemRunCtx<'_> {
    pub fn fetch_attachment(
        &self,
        attachment_id: &str,
    ) -> Result<Option<FetchedAttachment>, EwsError> {
        Ok(self.fetch_attachments(&[attachment_id])?.pop())
    }

    pub fn fetch_attachments(
        &self,
        attachment_ids: &[&str],
    ) -> Result<Vec<FetchedAttachment>, EwsError> {
        if attachment_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(attachment_ids.len());
        for chunk in attachment_ids.chunks(self.attachment_batch.max(1)) {
            let body = EwsRequest::GetAttachment { ids: chunk };
            let resp = self.client.call(self.url, &body)?;
            for att in GetAttachmentInline::parse_all(&resp.body)? {
                let bytes = LENIENT.decode(&att.content_base64).map_err(|e| {
                    EwsError::Malformed(format!("attachment {}: base64: {e}", att.attachment_id))
                })?;
                if bytes.is_empty() {
                    continue;
                }
                out.push(FetchedAttachment {
                    attachment_id: att.attachment_id,
                    bytes,
                    media_type: att
                        .content_type
                        .unwrap_or_else(|| FALLBACK_MEDIA_TYPE.to_owned()),
                });
            }
        }
        Ok(out)
    }
}
