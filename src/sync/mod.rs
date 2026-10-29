/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod emailmeta;
pub mod export;
pub mod import_dav;
pub mod import_exchange_ews;
pub mod import_exchange_graph;
pub mod import_imap;
pub mod import_jmap;
pub mod import_maildir;
pub mod import_managesieve;
pub mod import_takeout;
pub mod keys;
pub mod prune;

use std::path::PathBuf;

use rusqlite::Connection;

use crate::db;
use crate::error::Error;

pub(crate) fn table_name(ty: ObjectType) -> &'static str {
    match ty {
        ObjectType::Mailbox => "mailboxes",
        ObjectType::Email => "emails",
        ObjectType::Identity => "identities",
        ObjectType::SieveScript => "sieve_scripts",
        ObjectType::AddressBook => "address_books",
        ObjectType::ContactCard => "contact_cards",
        ObjectType::Calendar => "calendars",
        ObjectType::CalendarEvent => "calendar_events",
        ObjectType::ParticipantIdentity => "participant_identities",
        ObjectType::FileNode => "file_nodes",
    }
}
use crate::jmap::account::AccountSelector;
use crate::jmap::http::{Auth, HttpClient, RetryPolicy};
use crate::logging::Logger;
use crate::types::ObjectType;

pub struct CommonConfig {
    pub archive: PathBuf,
    pub threads: usize,
    pub dry_run: bool,
    pub max_retries: u32,
    pub allow_invalid_certs: bool,
    pub logger: Logger,
}

pub struct ConnectConfig {
    pub url: String,
    pub auth: Auth,
    pub account: AccountSelector,
}

pub struct ImportConfig {
    pub connect: ConnectConfig,
    pub objects: Option<Vec<ObjectType>>,
    pub allow_source_change: bool,
}

pub struct ExportConfig {
    pub connect: ConnectConfig,
    pub objects: Option<Vec<ObjectType>>,
    pub prune: bool,
    pub yes: bool,
}

#[derive(Debug, Default, Clone)]
pub struct TypeCounts {
    pub created: u64,
    pub fetched: u64,
    pub updated: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub failed: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub per_type: Vec<(&'static str, TypeCounts)>,
    pub retries_observed: u64,
    pub retry_after_sleeps: u64,
}

impl Summary {
    pub fn any_failed(&self) -> bool {
        self.per_type.iter().any(|(_, c)| c.failed > 0)
    }
}

#[derive(Debug)]
pub struct RunOutcome {
    pub summary: Summary,
    pub error: Option<Error>,
}

impl RunOutcome {
    pub fn from_result(result: Result<Summary, Error>) -> RunOutcome {
        match result {
            Ok(summary) => RunOutcome {
                summary,
                error: None,
            },
            Err(error) => RunOutcome {
                summary: Summary::default(),
                error: Some(error),
            },
        }
    }

    pub fn into_result(self) -> Result<Summary, Error> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.summary),
        }
    }
}

pub struct Context {
    pub conn: Connection,
    pub client: HttpClient,
    pub common: CommonConfig,
}

impl Context {
    pub fn open(common: CommonConfig, connect: &ConnectConfig) -> Result<Context, Error> {
        let conn = db::init::open(&common.archive)?;
        let client = HttpClient::new(
            connect.auth.clone(),
            RetryPolicy::new(common.max_retries),
            common.allow_invalid_certs,
        );
        Ok(Context {
            conn,
            client,
            common,
        })
    }

    pub fn dry_run(&self) -> bool {
        self.common.dry_run
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_aborted_run_keeps_its_error_and_its_partial_counts() {
        let mut summary = Summary::default();
        summary.per_type.push(("Mailbox", TypeCounts::default()));
        let outcome = RunOutcome {
            summary,
            error: Some(Error::Connection("http status 404".to_owned())),
        };
        assert_eq!(outcome.summary.per_type.len(), 1);
        let err = outcome.into_result().expect_err("aborted");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn a_result_without_a_native_outcome_reports_nothing_on_abort() {
        let outcome = RunOutcome::from_result(Err(Error::Partial("one object".to_owned())));
        assert!(outcome.summary.per_type.is_empty());
        assert!(outcome.error.is_some());
    }
}
