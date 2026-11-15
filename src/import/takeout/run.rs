/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use url::Url;

use crate::archive::sources::{SourceKey, SourceKind};
use crate::archive::{Archive, LocalId, SourceId};
use crate::console::{Counted, Grouped};
use crate::error::Error;
use crate::imap::automap::Automap;
use crate::logging::Logger;
use crate::net::http::RetryStats;
use crate::sync::run::{GcFailure, ImportRun, SourceClaim, SourceLabel};
use crate::sync::{RunOptions, RunOutcome, Summary, TypeCounts};
use crate::types::ObjectType;

use super::TakeoutImport;
use super::calendar::{CalendarFile, CalendarWriter};
use super::contacts::{ContactWriter, DEFAULT_ADDRESS_BOOK, VCardFile};
use super::mail::MailWriter;
use super::mbox::MessageIterator;
use super::walk::{FileKind, WalkResult};

const TYPES: [ObjectType; 6] = [
    ObjectType::Mailbox,
    ObjectType::Email,
    ObjectType::Calendar,
    ObjectType::CalendarEvent,
    ObjectType::AddressBook,
    ObjectType::ContactCard,
];

#[derive(Default)]
struct Counts {
    mailbox: TypeCounts,
    email: TypeCounts,
    calendar: TypeCounts,
    event: TypeCounts,
    book: TypeCounts,
    card: TypeCounts,
}

struct TakeoutRun {
    run: ImportRun,
    automap: Automap,
    counts: Counts,
}

impl TakeoutImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let failed = |error: Error| RunOutcome {
            summary: Summary::default(),
            error: Some(error),
        };
        let logger = options.logger;
        if options.threads > 1 {
            logger.progress(format_args!(
                "Takeout import runs on one thread; --threads is ignored"
            ));
        }
        let TakeoutImport {
            takeout_root,
            source_change,
            automap,
        } = self;
        let canonical = match TakeoutImport::scan_root(&takeout_root) {
            Ok(canonical) => canonical,
            Err(e) => return failed(e),
        };
        let walk = match WalkResult::scan(&canonical, logger) {
            Ok(walk) if walk.is_empty() => {
                return failed(Error::Usage(format!(
                    "{canonical:?}: no .mbox / .ics / .vcf files found under this path"
                )));
            }
            Ok(walk) => walk,
            Err(e) => return failed(Error::Usage(format!("walking {canonical:?}: {e}"))),
        };
        logger.progress(format_args!(
            "Found {} mbox, {} iCalendar and {} vCard files ({} unreadable, {} symlink cycles)",
            Grouped(walk.by_kind(FileKind::Mbox).count() as u64),
            Grouped(walk.by_kind(FileKind::Ics).count() as u64),
            Grouped(walk.by_kind(FileKind::Vcf).count() as u64),
            Grouped(walk.io_failures),
            Grouped(walk.symlink_cycles)
        ));
        let archive = match Archive::open(&options.archive) {
            Ok(archive) => archive,
            Err(e) => return failed(e.into()),
        };
        let session_url = match TakeoutImport::source_url(&canonical) {
            Ok(url) => url,
            Err(e) => return failed(e),
        };
        let account_id = canonical.to_string_lossy();
        let key = SourceKey {
            kind: SourceKind::Takeout,
            url: &session_url,
            account_id: &account_id,
        };
        let label = SourceLabel {
            name: canonical.file_name().and_then(|n| n.to_str()),
            username: "",
        };
        let run = match ImportRun::start_on(archive, options, &key, label, source_change) {
            Ok(run) => run,
            Err(e) => return failed(e),
        };
        let mut takeout = TakeoutRun {
            run,
            automap,
            counts: Counts::default(),
        };
        let result = takeout.execute(&walk);
        takeout.finish(result)
    }

    fn scan_root(root: &Path) -> Result<PathBuf, Error> {
        let canonical =
            fs::canonicalize(root).map_err(|e| Error::Usage(format!("--path {root:?}: {e}")))?;
        if !canonical.is_dir() {
            return Err(Error::Usage(format!("not a directory: {canonical:?}")));
        }
        Ok(canonical)
    }

    fn source_url(path: &Path) -> Result<String, Error> {
        Url::from_file_path(path).map(String::from).map_err(|_| {
            Error::Usage(format!(
                "cannot encode takeout path as file:// URL: {path:?}"
            ))
        })
    }
}

impl TakeoutRun {
    fn logger(&self) -> Logger {
        self.run.options().logger
    }

    fn execute(&mut self, walk: &WalkResult) -> Result<(), Error> {
        let source = match *self.run.claim() {
            SourceClaim::DryRun(source) => {
                self.dry_run(source, walk);
                return Ok(());
            }
            SourceClaim::Registered(source) => source,
        };
        self.import_mail(source, walk)?;
        self.import_calendars(source, walk)?;
        self.import_contacts(source, walk)
    }

    fn finish(self, result: Result<(), Error>) -> RunOutcome {
        let TakeoutRun {
            mut run, counts, ..
        } = self;
        let Counts {
            mailbox,
            email,
            calendar,
            event,
            book,
            card,
        } = counts;
        for (ty, counts) in TYPES
            .into_iter()
            .zip([mailbox, email, calendar, event, book, card])
        {
            *run.counts(ty) = counts;
        }
        match result {
            Ok(()) => run.finish(RetryStats::default(), GcFailure::Abort),
            Err(error) => run.abort(error, RetryStats::default()),
        }
    }

    fn import_mail(&mut self, source: SourceId, walk: &WalkResult) -> Result<(), Error> {
        let logger = self.logger();
        let mut mailboxes = self
            .run
            .store()
            .takeout_ids(source)
            .all(ObjectType::Mailbox)?;
        for file in walk.by_kind(FileKind::Mbox) {
            let fallback = MailWriter::fallback_mailbox(&file.path);
            logger.progress(format_args!(
                "Reading mail from {} (unlabelled messages go to \"{fallback}\")",
                FileName(&file.path)
            ));
            if let Err(e) = self.import_mbox(source, &file.path, fallback, &mut mailboxes) {
                if e.aborts_run() {
                    return Err(e);
                }
                logger.warn(format_args!("cannot import {}: {e}", file.path.display()));
                self.counts.email.failed += 1;
            }
        }
        Ok(())
    }

    fn import_mbox(
        &mut self,
        source: SourceId,
        path: &Path,
        fallback: &str,
        mailboxes: &mut HashMap<String, LocalId>,
    ) -> Result<(), Error> {
        let messages = MessageIterator::open(path)?;
        let logger = self.logger();
        let automap = self.automap;
        let tx = self.run.archive().transaction()?;
        let store = tx.store();
        MailWriter {
            store,
            ids: store.takeout_ids(source),
            automap,
            fallback,
            mailboxes,
            mailbox_counts: &mut self.counts.mailbox,
            email_counts: &mut self.counts.email,
            logger,
        }
        .write_all(path, messages);
        tx.commit()?;
        Ok(())
    }

    fn import_calendars(&mut self, source: SourceId, walk: &WalkResult) -> Result<(), Error> {
        let logger = self.logger();
        let mut calendars = self
            .run
            .store()
            .takeout_ids(source)
            .all(ObjectType::Calendar)?;
        for file in walk.by_kind(FileKind::Ics) {
            logger.progress(format_args!("Reading events from {}", FileName(&file.path)));
            if let Err(e) = self.import_ics(source, &file.path, &mut calendars) {
                if e.aborts_run() {
                    return Err(e);
                }
                logger.warn(format_args!("cannot import {}: {e}", file.path.display()));
                self.counts.event.failed += 1;
            }
        }
        Ok(())
    }

    fn import_ics(
        &mut self,
        source: SourceId,
        path: &Path,
        calendars: &mut HashMap<String, LocalId>,
    ) -> Result<(), Error> {
        let logger = self.logger();
        let Some(file) = CalendarFile::load(path, &mut self.counts.event, logger) else {
            return Ok(());
        };
        let tx = self.run.archive().transaction()?;
        let store = tx.store();
        CalendarWriter {
            store,
            ids: store.takeout_ids(source),
            calendars,
            calendar_counts: &mut self.counts.calendar,
            event_counts: &mut self.counts.event,
            logger,
        }
        .write(path, file)?;
        tx.commit()?;
        Ok(())
    }

    fn import_contacts(&mut self, source: SourceId, walk: &WalkResult) -> Result<(), Error> {
        let logger = self.logger();
        let mut book = self
            .run
            .store()
            .takeout_ids(source)
            .local_id(ObjectType::AddressBook, DEFAULT_ADDRESS_BOOK)?;
        for file in walk.by_kind(FileKind::Vcf) {
            logger.progress(format_args!(
                "Reading contacts from {}",
                FileName(&file.path)
            ));
            if let Err(e) = self.import_vcf(source, &file.path, &mut book) {
                if e.aborts_run() {
                    return Err(e);
                }
                logger.warn(format_args!("cannot import {}: {e}", file.path.display()));
                self.counts.card.failed += 1;
            }
        }
        Ok(())
    }

    fn import_vcf(
        &mut self,
        source: SourceId,
        path: &Path,
        book: &mut Option<LocalId>,
    ) -> Result<(), Error> {
        let logger = self.logger();
        let chunks = VCardFile::load(path, &mut self.counts.card, logger);
        if chunks.is_empty() {
            return Ok(());
        }
        let tx = self.run.archive().transaction()?;
        let store = tx.store();
        ContactWriter {
            store,
            ids: store.takeout_ids(source),
            book,
            book_counts: &mut self.counts.book,
            card_counts: &mut self.counts.card,
            logger,
        }
        .write(path, &chunks)?;
        tx.commit()?;
        Ok(())
    }

    fn dry_run(&self, source: Option<SourceId>, walk: &WalkResult) {
        let ids = source.map(|source| self.run.store().takeout_ids(source));
        let existing = |ty: ObjectType| ids.map_or(0, |ids| ids.count(ty).unwrap_or(0));
        self.logger().note(format_args!(
            "a Takeout dry run does not parse files; found {} mbox, {} iCalendar and {} vCard files, \
             and the archive holds {}, {} and {}",
            Grouped(walk.by_kind(FileKind::Mbox).count() as u64),
            Grouped(walk.by_kind(FileKind::Ics).count() as u64),
            Grouped(walk.by_kind(FileKind::Vcf).count() as u64),
            Counted::new(existing(ObjectType::Email), "email", "emails"),
            Counted::new(
                existing(ObjectType::CalendarEvent),
                "calendar event",
                "calendar events"
            ),
            Counted::new(existing(ObjectType::ContactCard), "contact card", "contact cards")
        ));
    }
}

struct FileName<'p>(&'p Path);

impl fmt::Display for FileName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.file_name() {
            Some(name) => name.display().fmt(f),
            None => self.0.display().fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_encodes_unicode_path() {
        let u = TakeoutImport::source_url(Path::new("/tmp/Año")).unwrap();
        assert!(u.starts_with("file://"));
        assert!(u.contains("A%C3%B1o"));
    }

    // Progress lines name a file by its last component, or the whole path when it has none.
    #[test]
    fn file_name_shows_the_last_component() {
        assert_eq!(
            FileName(Path::new("/t/Mail/All mail.mbox")).to_string(),
            "All mail.mbox"
        );
        assert_eq!(FileName(Path::new("/")).to_string(), "/");
    }

    #[test]
    fn file_url_rejects_relative_path() {
        let err = TakeoutImport::source_url(Path::new("./relative")).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }
}
