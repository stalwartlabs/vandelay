/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value, json};

use crate::export::Exporter;
use crate::export::target::{ClientId, TargetJson};
use crate::jmap::error::JmapError;
use crate::jmap::request::{MethodCallKind, Request};

pub(super) enum SingleImport {
    Created,
    Skipped,
    NotCreated { error_type: String, detail: String },
}

impl SingleImport {
    fn from_response(args: &Value, cid: &str) -> SingleImport {
        if let Some(err) = args
            .get("notCreated")
            .and_then(Value::as_object)
            .and_then(|nc| nc.get(cid))
        {
            let error_type = TargetJson(err).error_type().unwrap_or_default();
            if error_type == "alreadyExists" {
                return SingleImport::Skipped;
            }
            return SingleImport::NotCreated {
                error_type: error_type.to_owned(),
                detail: err.to_string(),
            };
        }
        if args
            .get("created")
            .and_then(Value::as_object)
            .is_some_and(|c| c.contains_key(cid))
        {
            return SingleImport::Created;
        }
        SingleImport::NotCreated {
            error_type: String::new(),
            detail: format!("Email/import returned neither created nor notCreated for {cid}"),
        }
    }
}

impl Exporter<'_> {
    fn import_request(&self, items: &[(ClientId, Value)]) -> Request {
        let emails: Map<String, Value> = items
            .iter()
            .map(|(cid, item)| (cid.as_str().to_owned(), item.clone()))
            .collect();
        let mut req = Request::new();
        req.call(
            "Email/import",
            json!({ "accountId": self.account.account_id(), "emails": Value::Object(emails) }),
            "i",
        );
        req
    }

    pub(super) fn send_import(
        &self,
        items: &[(ClientId, Value)],
        out: &mut Vec<Result<SingleImport, JmapError>>,
    ) -> Result<(), JmapError> {
        let req = self.import_request(items);
        let sent = req.fits(self.account.limits()).and_then(|()| {
            self.account
                .client()
                .retry_method(MethodCallKind::SingleObjectWrite, || {
                    let resp = self.account.send(&req)?;
                    let mr = resp.first()?;
                    mr.check_error()?;
                    Ok(items
                        .iter()
                        .map(|(cid, _)| SingleImport::from_response(&mr.args, cid.as_str()))
                        .collect::<Vec<_>>())
                })
        });
        match sent {
            Ok(outcomes) => {
                out.extend(outcomes.into_iter().map(Ok));
                Ok(())
            }
            Err(JmapError::RequestTooLarge) if items.len() > 1 => {
                let (left, right) = items.split_at(items.len() / 2);
                self.send_import(left, out)?;
                self.send_import(right, out)
            }
            Err(e @ JmapError::RequestTooLarge) => {
                out.push(Err(e));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_outcome_reads_each_creation_id() {
        let args = json!({
            "created": { "e1": { "id": "M1" } },
            "notCreated": {
                "e2": { "type": "alreadyExists", "existingId": "M9" },
                "e3": { "type": "blobNotFound" },
                "e4": { "type": "overQuota" }
            }
        });
        assert!(matches!(
            SingleImport::from_response(&args, "e1"),
            SingleImport::Created
        ));
        assert!(matches!(
            SingleImport::from_response(&args, "e2"),
            SingleImport::Skipped
        ));
        assert!(matches!(
            SingleImport::from_response(&args, "e3"),
            SingleImport::NotCreated { ref error_type, .. } if error_type == "blobNotFound"
        ));
        assert!(matches!(
            SingleImport::from_response(&args, "e4"),
            SingleImport::NotCreated { ref error_type, .. } if error_type == "overQuota"
        ));
        assert!(matches!(
            SingleImport::from_response(&args, "e5"),
            SingleImport::NotCreated { ref error_type, .. } if error_type.is_empty()
        ));
    }
}
