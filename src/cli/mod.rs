/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod auth;
mod dav;
mod ews;
mod export;
mod global;
mod graph;
mod imap;
mod inspect;
mod jmap;
mod maildir;
mod managesieve;
mod report;
mod takeout;

use std::io::{self, IsTerminal};
use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use regex::Regex;

use self::dav::DavImportArgs;
use self::ews::ExchangeEwsImportArgs;
use self::export::ExportArgs;
use self::graph::ExchangeGraphImportArgs;
use self::imap::ImapImportArgs;
use self::inspect::InspectArgs;
use self::jmap::JmapImportArgs;
use self::maildir::MaildirImportArgs;
use self::managesieve::ManageSieveImportArgs;
use self::report::{Report, RunKind};
use self::takeout::TakeoutImportArgs;
use crate::console::{ColorChoice, Console, Counted, Logo, Severity};
use crate::error::{Error, ExitStatus};
use crate::export::Export;
use crate::import::Import;
use crate::import::dav::DavKindArg;
use crate::inspect::Inspect;
use crate::logging::{Level, Logger};
use crate::sync::{RunMode, RunOptions};

#[derive(Parser)]
#[command(
    name = "vandelay",
    version,
    about = "Vandelay: the JMAP importer-exporter"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "WHEN",
        default_value = "auto",
        help = "Colour output: auto (terminal only, honours NO_COLOR), always or never"
    )]
    color: ColorChoice,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Read a source account into a local archive")]
    Import {
        #[command(subcommand)]
        source: Source,
    },
    #[command(about = "Push a local archive into a target JMAP server")]
    Export(Box<ExportArgs>),
    #[command(about = "Show the contents of a local archive (read-only)")]
    Inspect(Box<InspectArgs>),
}

#[derive(Subcommand)]
enum Source {
    #[command(about = "JMAP server")]
    Jmap(Box<JmapImportArgs>),
    #[command(about = "IMAP server")]
    Imap(Box<ImapImportArgs>),
    #[command(about = "CalDAV server")]
    Caldav(Box<DavImportArgs>),
    #[command(about = "CardDAV server")]
    Carddav(Box<DavImportArgs>),
    #[command(about = "WebDAV server (plain files)")]
    Webdav(Box<DavImportArgs>),
    #[command(about = "ManageSieve server (Sieve scripts only)")]
    Managesieve(Box<ManageSieveImportArgs>),
    #[command(about = "Local Maildir++ tree")]
    Maildir(Box<MaildirImportArgs>),
    #[command(about = "Google Takeout (or any mbox/ics/vcf directory tree)")]
    Takeout(Box<TakeoutImportArgs>),
    #[command(about = "MS Exchange via EWS (SOAP/XML)", name = "exchange-ews")]
    ExchangeEws(Box<ExchangeEwsImportArgs>),
    #[command(
        about = "MS Exchange Online via Microsoft Graph",
        name = "exchange-graph"
    )]
    ExchangeGraph(Box<ExchangeGraphImportArgs>),
}

pub enum Action {
    Import(RunOptions, Import),
    Export(RunOptions, Export),
    Inspect(Inspect),
}

impl Action {
    pub fn import(options: RunOptions, import: impl Into<Import>) -> Action {
        Action::Import(options, import.into())
    }

    pub fn with_console(self, console: Console) -> Action {
        match self {
            Action::Import(mut options, import) => {
                options.logger = options.logger.with_console(console);
                Action::Import(options, import)
            }
            Action::Export(mut options, export) => {
                options.logger = options.logger.with_console(console);
                Action::Export(options, export)
            }
            Action::Inspect(inspect) => Action::Inspect(Inspect { console, ..inspect }),
        }
    }

    pub fn run(self) -> ExitStatus {
        let (kind, options, outcome) = match self {
            Action::Import(options, import) => {
                let run = RunHeader::start(&options);
                (RunKind::Import, run, import.run(options))
            }
            Action::Export(options, export) => {
                let run = RunHeader::start(&options);
                (RunKind::Export(export.prune), run, export.run(options))
            }
            Action::Inspect(inspect) => {
                let console = inspect.console;
                return match inspect.run() {
                    Ok(()) => ExitStatus::Ok,
                    Err(err) => Action::fail(console, &err),
                };
            }
        };
        let RunHeader {
            logger,
            archive,
            mode,
        } = options;
        let console = logger.console();
        let _ = Report {
            kind,
            archive: &archive,
            mode,
            summary: &outcome.summary,
        }
        .write(console.out, &mut io::stdout().lock());
        let failed = outcome.summary.failed();
        match outcome.error {
            Some(err) => Action::fail(console, &err),
            None if failed > 0 => {
                logger.error(format_args!(
                    "{} failed; the archive is consistent and a re-run resumes",
                    Counted::new(failed, "object", "objects")
                ));
                ExitStatus::Partial
            }
            None => ExitStatus::Ok,
        }
    }

    pub fn fail(console: Console, err: &Error) -> ExitStatus {
        eprintln!("{} {err}", console.err.severity(Severity::Error));
        err.exit_status()
    }
}

struct RunHeader {
    logger: Logger,
    archive: PathBuf,
    mode: RunMode,
}

impl RunHeader {
    fn start(options: &RunOptions) -> RunHeader {
        let logger = options.logger;
        if logger.enabled(Level::Default) && io::stderr().is_terminal() {
            eprintln!("{}\n", Logo::for_stderr().text(logger.console().err));
        }
        RunHeader {
            logger,
            archive: options.archive.clone(),
            mode: options.mode,
        }
    }
}

impl Cli {
    pub fn try_parse_styled(choice: ColorChoice, console: Console) -> Result<Cli, clap::Error> {
        let logo = Logo::for_stdout().text(console.out);
        let matches = Cli::command()
            .color(choice.clap())
            .before_help(logo)
            .before_long_help(logo)
            .try_get_matches()?;
        Cli::from_arg_matches(&matches)
    }

    pub fn resolve(self) -> Result<Action, Error> {
        match self.command {
            Command::Import { source } => source.resolve(),
            Command::Export(args) => args.resolve(),
            Command::Inspect(args) => args.resolve(),
        }
    }
}

impl Source {
    fn resolve(self) -> Result<Action, Error> {
        match self {
            Source::Jmap(args) => args.resolve(),
            Source::Imap(args) => args.resolve(),
            Source::Caldav(args) => args.resolve(DavKindArg::Caldav),
            Source::Carddav(args) => args.resolve(DavKindArg::Carddav),
            Source::Webdav(args) => args.resolve(DavKindArg::Webdav),
            Source::Managesieve(args) => args.resolve(),
            Source::Maildir(args) => args.resolve(),
            Source::Takeout(args) => args.resolve(),
            Source::ExchangeEws(args) => args.resolve(),
            Source::ExchangeGraph(args) => args.resolve(),
        }
    }
}

#[derive(Clone, Copy)]
enum FolderFilter {
    Include,
    Exclude,
}

impl FolderFilter {
    fn flag(self) -> &'static str {
        match self {
            FolderFilter::Include => "--include",
            FolderFilter::Exclude => "--exclude",
        }
    }

    fn compile(self, raw: &[String]) -> Result<Vec<Regex>, Error> {
        raw.iter()
            .map(|r| {
                Regex::new(r)
                    .map_err(|e| Error::Usage(format!("{} {r:?}: invalid regex: {e}", self.flag())))
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
    M.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    // an invalid pattern is a usage error naming its flag; valid ones compile in order
    #[test]
    fn folder_filter_names_the_flag_of_an_invalid_pattern() {
        let err = FolderFilter::Exclude
            .compile(&["^a".to_owned(), "(".to_owned()])
            .unwrap_err();
        assert!(
            matches!(&err, Error::Usage(m) if m.starts_with("--exclude \"(\": invalid regex: ")),
            "{err}"
        );
        let compiled = FolderFilter::Include
            .compile(&["^a".to_owned(), "b$".to_owned()])
            .unwrap();
        assert!(compiled.iter().map(Regex::as_str).eq(["^a", "b$"]));
    }
}
