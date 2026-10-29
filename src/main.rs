/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use clap::Parser;

use vandelay::cli::{Action, Cli};
use vandelay::error::Error;
use vandelay::inspect;
use vandelay::sync::{self, RunOutcome, Summary};

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return if err.use_stderr() { 1 } else { 0 };
        }
    };

    let action = match cli.resolve() {
        Ok(action) => action,
        Err(err) => return fail(&err),
    };

    let (outcome, logger) = match action {
        Action::Import(common, config) => {
            let logger = common.logger;
            (sync::import_jmap::run_reporting(common, config), logger)
        }
        Action::ImportImap(common, config) => {
            let logger = common.logger;
            (sync::import_imap::run_reporting(common, config), logger)
        }
        Action::ImportDav(common, config) => {
            let logger = common.logger;
            (sync::import_dav::run_reporting(common, config), logger)
        }
        Action::ImportManageSieve(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::import_managesieve::run(common, config)),
                logger,
            )
        }
        Action::ImportMaildir(common, config) => {
            let logger = common.logger;
            (sync::import_maildir::run_reporting(common, config), logger)
        }
        Action::ImportTakeout(common, config) => {
            let logger = common.logger;
            (sync::import_takeout::run_reporting(common, config), logger)
        }
        Action::ImportExchangeEws(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::import_exchange_ews::run(common, config)),
                logger,
            )
        }
        Action::ImportExchangeGraph(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::import_exchange_graph::run(common, config)),
                logger,
            )
        }
        Action::Export(common, config) => {
            let logger = common.logger;
            (
                RunOutcome::from_result(sync::export::run(common, config)),
                logger,
            )
        }
        Action::Inspect(config) => {
            return match inspect::run(config) {
                Ok(()) => 0,
                Err(err) => fail(&err),
            };
        }
    };

    report(&outcome.summary);
    match outcome.error {
        Some(err) => fail(&err),
        None => {
            if outcome.summary.any_failed() {
                logger.error("some objects failed; the archive is consistent and resumable");
                5
            } else {
                0
            }
        }
    }
}

fn fail(err: &Error) -> i32 {
    eprintln!("error: {err}");
    err.exit_code()
}

fn report(summary: &Summary) {
    for (type_name, counts) in &summary.per_type {
        println!(
            "{type_name}: created={} fetched={} updated={} deleted={} skipped={} failed={}",
            counts.created,
            counts.fetched,
            counts.updated,
            counts.deleted,
            counts.skipped,
            counts.failed
        );
    }
}
