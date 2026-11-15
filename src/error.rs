/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Display;
use std::io;

use crate::archive::ArchiveError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),

    #[error("connection failed: {0}")]
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

    #[error("archive: {0}")]
    Db(#[from] ArchiveError),

    #[error("i/o: {0}")]
    Io(#[from] io::Error),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(ArchiveError::Sqlite(e))
    }
}

impl Error {
    pub fn partial(e: impl Display) -> Error {
        Error::Partial(e.to_string())
    }

    pub fn connection(e: impl Display) -> Error {
        Error::Connection(e.to_string())
    }

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

    pub fn exit_status(&self) -> ExitStatus {
        match self {
            Error::Usage(_) | Error::Unimplemented(_) => ExitStatus::Usage,
            Error::Connection(_) => ExitStatus::Connection,
            Error::Account(_) => ExitStatus::Account,
            Error::SourceChange(_) => ExitStatus::SourceChange,
            Error::Partial(_) => ExitStatus::Partial,
            Error::PruneAborted => ExitStatus::PruneAborted,
            Error::Db(_) | Error::Io(_) => ExitStatus::Archive,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitStatus {
    Ok = 0,
    Usage = 1,
    Connection = 2,
    Account = 3,
    SourceChange = 4,
    Partial = 5,
    PruneAborted = 6,
    Archive = 7,
}

impl ExitStatus {
    pub fn code(self) -> i32 {
        self as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_cli_md_table() {
        assert_eq!(Error::Usage("x".into()).exit_status(), ExitStatus::Usage);
        assert_eq!(Error::Unimplemented("x").exit_status(), ExitStatus::Usage);
        assert_eq!(
            Error::Connection("x".into()).exit_status(),
            ExitStatus::Connection
        );
        assert_eq!(
            Error::Account("x".into()).exit_status(),
            ExitStatus::Account
        );
        assert_eq!(
            Error::SourceChange("x".into()).exit_status(),
            ExitStatus::SourceChange
        );
        assert_eq!(
            Error::Partial("x".into()).exit_status(),
            ExitStatus::Partial
        );
        assert_eq!(Error::PruneAborted.exit_status(), ExitStatus::PruneAborted);
        let io = Error::Io(io::Error::other("disk"));
        assert_eq!(io.exit_status(), ExitStatus::Archive);
    }

    #[test]
    fn exit_status_codes_are_stable() {
        let codes = [
            ExitStatus::Ok,
            ExitStatus::Usage,
            ExitStatus::Connection,
            ExitStatus::Account,
            ExitStatus::SourceChange,
            ExitStatus::Partial,
            ExitStatus::PruneAborted,
            ExitStatus::Archive,
        ]
        .map(ExitStatus::code);
        assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6, 7]);
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
        assert!(Error::Db(ArchiveError::Sqlite(rusqlite::Error::QueryReturnedNoRows)).aborts_run());
        assert!(Error::Io(io::Error::other("disk")).aborts_run());
    }

    #[test]
    fn aborting_errors_do_not_map_to_the_partial_exit_code() {
        for e in [
            Error::Connection("x".into()),
            Error::Account("x".into()),
            Error::SourceChange("x".into()),
            Error::Db(ArchiveError::Sqlite(rusqlite::Error::QueryReturnedNoRows)),
            Error::Io(io::Error::other("disk")),
        ] {
            assert!(e.aborts_run(), "{e} must abort the run");
            assert_ne!(
                e.exit_status(),
                ExitStatus::Partial,
                "{e} must not report a partial failure"
            );
        }
        assert_eq!(
            Error::Connection("x".into()).exit_status(),
            ExitStatus::Connection
        );
        assert_eq!(
            Error::Partial("x".into()).exit_status(),
            ExitStatus::Partial
        );
    }
}
