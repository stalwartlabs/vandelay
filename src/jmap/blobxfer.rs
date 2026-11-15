/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::Value;

use crate::jmap::connect::JmapAccount;
use crate::jmap::error::JmapError;
use crate::jmap::wire::JmapId;

impl JmapAccount {
    pub fn upload(&self, bytes: &[u8], content_type: &str) -> Result<JmapId, JmapError> {
        let url = self.session().upload_url_for(self.account_id());
        let response = self.client().upload(&url, content_type, bytes)?;
        response
            .get("blobId")
            .and_then(Value::as_str)
            .map(|s| JmapId(s.to_owned()))
            .ok_or_else(|| JmapError::malformed("upload response has no blobId"))
    }

    pub fn download(
        &self,
        blob_id: &str,
        type_hint: &str,
        name: &str,
    ) -> Result<Vec<u8>, JmapError> {
        let url = self
            .session()
            .download_url_for(self.account_id(), blob_id, type_hint, name);
        self.client().download(&url)
    }
}
