/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::imap::client::{Capability, ImapClient};
use crate::imap::command::Command;
use crate::imap::error::ImapError;
use crate::imap::response::{Untagged, Value};

pub struct UidSearch;

impl UidSearch {
    pub fn all(client: &mut ImapClient) -> Result<Vec<u32>, ImapError> {
        if client.has(Capability::Esearch) {
            match client.run_collect(&Command::UidSearchEsearchAll) {
                Ok(resp) => {
                    if let Some(all) = resp.untagged.into_iter().find_map(|u| match u {
                        Untagged::Esearch { all, .. } => Some(all),
                        _ => None,
                    }) {
                        return Ok(all);
                    }
                }
                Err(e) if e.is_negotiation_failure() => {}
                Err(e) => return Err(e),
            }
        }
        match client.run_collect(&Command::UidSearchAll) {
            Ok(resp) => Ok(resp
                .untagged
                .into_iter()
                .find_map(|u| match u {
                    Untagged::Search(uids) => Some(uids),
                    _ => None,
                })
                .unwrap_or_default()),
            Err(e) if e.is_negotiation_failure() => UidSearch::via_fetch(client),
            Err(e) => Err(e),
        }
    }

    fn via_fetch(client: &mut ImapClient) -> Result<Vec<u32>, ImapError> {
        let resp = client.run_collect(&Command::UidFetchAllUids)?;
        let mut out = Vec::new();
        for u in resp.untagged {
            if let Untagged::Fetch { items, .. } = u {
                out.extend(items.into_iter().filter_map(|(n, v)| match v {
                    Value::Number(num) if n == "UID" => u32::try_from(num).ok(),
                    _ => None,
                }));
            }
        }
        Ok(out)
    }
}
