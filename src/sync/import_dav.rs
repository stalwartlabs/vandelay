/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod calcard;
pub mod collections;
pub mod coordinator;
pub mod items;
pub mod tree;

pub use coordinator::{DavAuth, DavImportConfig, DavKindArg, run, run_reporting};

use crate::error::Error;
use crate::jmap::error::JmapError;

pub(crate) fn per_collection_failure(err: JmapError) -> Error {
    match err {
        JmapError::Sqlite(e) => Error::Db(crate::db::init::OpenError::Sqlite(e)),
        scoped => Error::Partial(scoped.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forbidden_collection_is_a_per_unit_failure() {
        for e in [
            JmapError::Auth("server returned 403: forbidden".to_owned()),
            JmapError::HttpStatus {
                status: 405,
                body: "method not allowed".to_owned(),
            },
            JmapError::RetriesExhausted("PROPFIND kept returning 503".to_owned()),
            JmapError::Transport("io: connection reset".to_owned()),
            JmapError::Malformed("multistatus parse".to_owned()),
        ] {
            let mapped = per_collection_failure(e);
            assert!(
                !mapped.aborts_run(),
                "{mapped} is scoped to one collection and must not abort the run"
            );
            assert_eq!(mapped.exit_code(), 5);
        }
    }

    #[test]
    fn an_archive_failure_still_aborts() {
        let mapped =
            per_collection_failure(JmapError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
        assert!(mapped.aborts_run());
        assert_eq!(mapped.exit_code(), 7);
    }
}
