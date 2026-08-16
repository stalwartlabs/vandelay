/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::db::init::OpenError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("usage error: {0}")]
    Usage(String),

    #[error("connection error: {0}")]
    Connection(String),

    #[error("account resolution failed: {0}")]
    Account(String),

    #[error("source-change protection: {0}")]
    SourceChange(String),

    #[error("partial failure: {0}")]
    Partial(String),

    #[error("aborted at prune confirmation")]
    PruneAborted,

    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),

    #[error("archive error: {0}")]
    Db(#[from] OpenError),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(OpenError::Sqlite(e))
    }
}

impl Error {
    pub fn aborts_run(&self) -> bool {
        match self {
            Error::Partial(_) => false,
            Error::Usage(_)
            | Error::Connection(_)
            | Error::Account(_)
            | Error::SourceChange(_)
            | Error::PruneAborted
            | Error::Unimplemented(_)
            | Error::Db(_)
            | Error::Io(_) => true,
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 1,
            Error::Connection(_) => 2,
            Error::Account(_) => 3,
            Error::SourceChange(_) => 4,
            Error::Partial(_) => 5,
            Error::PruneAborted => 6,
            Error::Unimplemented(_) => 1,
            Error::Db(_) => 7,
            Error::Io(_) => 7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_cli_md_table() {
        assert_eq!(Error::Usage("x".into()).exit_code(), 1);
        assert_eq!(Error::Unimplemented("x").exit_code(), 1);
        assert_eq!(Error::Connection("x".into()).exit_code(), 2);
        assert_eq!(Error::Account("x".into()).exit_code(), 3);
        assert_eq!(Error::SourceChange("x".into()).exit_code(), 4);
        assert_eq!(Error::Partial("x".into()).exit_code(), 5);
        assert_eq!(Error::PruneAborted.exit_code(), 6);
        let io = Error::Io(std::io::Error::other("disk"));
        assert_eq!(io.exit_code(), 7);
    }

    #[test]
    fn only_partial_is_a_per_unit_failure() {
        assert!(!Error::Partial("one object".into()).aborts_run());
    }

    #[test]
    fn whole_run_conditions_abort() {
        assert!(Error::Connection("http status 404".into()).aborts_run());
        assert!(Error::Account("ambiguous".into()).aborts_run());
        assert!(Error::SourceChange("other account".into()).aborts_run());
        assert!(Error::Usage("bad flag".into()).aborts_run());
        assert!(Error::Unimplemented("x").aborts_run());
        assert!(Error::PruneAborted.aborts_run());
        assert!(Error::Db(OpenError::Sqlite(rusqlite::Error::QueryReturnedNoRows)).aborts_run());
        assert!(Error::Io(std::io::Error::other("disk")).aborts_run());
    }

    #[test]
    fn aborting_errors_do_not_map_to_the_partial_exit_code() {
        for e in [
            Error::Connection("x".into()),
            Error::Account("x".into()),
            Error::SourceChange("x".into()),
            Error::Db(OpenError::Sqlite(rusqlite::Error::QueryReturnedNoRows)),
            Error::Io(std::io::Error::other("disk")),
        ] {
            assert!(e.aborts_run(), "{e} must abort the run");
            assert_ne!(e.exit_code(), 5, "{e} must not report a partial failure");
        }
        assert_eq!(Error::Connection("x".into()).exit_code(), 2);
        assert_eq!(Error::Partial("x".into()).exit_code(), 5);
    }
}
