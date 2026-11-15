/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::PathBuf;

use clap::{ArgAction, Args};

use crate::logging::{Logger, Verbosity};
use crate::net::http::TlsPolicy;
use crate::sync::{RunMode, RunOptions};

#[derive(Args)]
pub(super) struct GlobalArgs {
    #[arg(
        short = 'j',
        long,
        value_name = "N",
        help = "Worker pool size (default: logical CPUs)"
    )]
    threads: Option<usize>,

    #[arg(long, help = "Compute and report the full plan; perform no writes")]
    dry_run: bool,

    #[arg(
        short = 'v',
        long,
        action = ArgAction::Count,
        conflicts_with = "quiet",
        help = "Increase log verbosity (repeatable, max -vvv)",
        long_help = "Increase log verbosity (repeatable, max -vvv).\n  \
            (default) per-type start/finish lines and a final summary\n  \
            -v   per-chunk progress, account resolution, retry notices\n  \
            -vv  every protocol call (method/command, target, status, timing)\n  \
            -vvv full request/response bodies and backoff delays (stderr)"
    )]
    verbose: u8,

    #[arg(
        short = 'q',
        long,
        help = "Quiet: warnings and errors only (verbosity level 0)"
    )]
    quiet: bool,

    #[arg(
        long,
        value_name = "N",
        default_value_t = 5,
        help = "Max retry attempts per request on transient failures"
    )]
    max_retries: u32,

    #[arg(long, help = "Accept self-signed / invalid TLS certificates")]
    allow_invalid_certs: bool,
}

impl GlobalArgs {
    pub(super) fn common_config(&self, archive: PathBuf) -> RunOptions {
        let threads = self
            .threads
            .filter(|n| *n > 0)
            .unwrap_or_else(num_cpus::get)
            .max(1);
        RunOptions {
            archive,
            threads,
            mode: if self.dry_run {
                RunMode::DryRun
            } else {
                RunMode::Apply
            },
            max_retries: self.max_retries,
            tls: if self.allow_invalid_certs {
                TlsPolicy::AcceptInvalid
            } else {
                TlsPolicy::Verify
            },
            logger: Logger::from_verbosity(if self.quiet {
                Verbosity::Quiet
            } else {
                Verbosity::Verbose(self.verbose)
            }),
        }
    }
}
