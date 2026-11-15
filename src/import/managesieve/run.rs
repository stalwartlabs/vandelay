/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::archive::sources::{SourceKey, SourceKind};
use crate::archive::{Archive, LocalId, SourceId};
use crate::error::Error;
use crate::managesieve::session::{SIEVE, SieveSession};
use crate::net::backoff::{Fault, Retryable};
use crate::net::stream::{Connector, Endpoint};
use crate::sync::run::{GcFailure, ImportRun, SourceClaim, SourceLabel};
use crate::sync::{RunOptions, RunOutcome, Summary, TypeCounts};
use crate::types::ObjectType;

use super::ManageSieveImport;
use super::control::Control;
use super::plan::{Action, LocalScripts, Plan, PresentOutcome, ScriptWriter, ServerScript};

struct SieveRun {
    run: ImportRun,
    control: Control,
    counts: TypeCounts,
}

impl ManageSieveImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let failed = |error: Error| RunOutcome {
            summary: Summary::default(),
            error: Some(error),
        };
        let logger = options.logger;
        if options.threads > 1 {
            logger.progress(format_args!(
                "ManageSieve import runs on one thread; --threads is ignored"
            ));
        }
        let archive = match Archive::open(&options.archive) {
            Ok(archive) => archive,
            Err(e) => return failed(e.into()),
        };
        let ManageSieveImport {
            url,
            auth,
            cleartext,
            source_change,
        } = self;
        let session = match Endpoint::parse(&url, &SIEVE).and_then(|endpoint| {
            let connector = Connector::new(options.tls).map_err(Error::connection)?;
            Ok(SieveSession::new(
                connector, endpoint, cleartext, auth, logger,
            ))
        }) {
            Ok(session) => session,
            Err(e) => return failed(e),
        };
        let client = match session.open() {
            Ok(client) => client,
            Err(e) => return failed(e),
        };
        let session_url = session.session_url();
        let account_id = session.account_id().to_owned();
        let key = SourceKey {
            kind: SourceKind::ManageSieve,
            url: &session_url,
            account_id: &account_id,
        };
        let label = SourceLabel {
            name: Some(&account_id),
            username: &account_id,
        };
        let max_retries = options.max_retries;
        let run = match ImportRun::start_on(archive, options, &key, label, source_change) {
            Ok(run) => run,
            Err(e) => return failed(e),
        };
        let mut sieve = SieveRun {
            run,
            control: Control::new(client, session, max_retries),
            counts: TypeCounts::default(),
        };
        let result = sieve.execute();
        sieve.finish(result)
    }
}

impl SieveRun {
    fn execute(&mut self) -> Result<(), Error> {
        let server = self.fetch_scripts()?;
        match *self.run.claim() {
            SourceClaim::DryRun(source) => self.dry_run(source, &server),
            SourceClaim::Registered(source) => self.apply(source, &server),
        }
    }

    fn fetch_scripts(&mut self) -> Result<Vec<ServerScript>, Error> {
        let listing = self.control.listscripts()?;
        let logger = self.run.options().logger;
        let mut server = Vec::with_capacity(listing.len());
        for listed in listing {
            let name = listed.name;
            match self.control.getscript(&name) {
                Ok(bytes) => server.push(ServerScript::new(name, listed.active, bytes)),
                Err(e) if e.is_referral() => {
                    return Err(Error::Connection(format!(
                        "ManageSieve referral on GETSCRIPT {name:?}: {e}; vandelay does not follow referrals"
                    )));
                }
                Err(e) if e.fault() == Fault::TransportDrop => {
                    return Err(Error::Connection(format!(
                        "GETSCRIPT {name:?} transport drop exhausted retries: {e}"
                    )));
                }
                Err(e) => {
                    logger.warn(format_args!("GETSCRIPT \"{name}\" failed; skipped: {e}"));
                    self.counts.failed += 1;
                }
            }
        }
        Ok(server)
    }

    fn dry_run(&mut self, source: Option<SourceId>, server: &[ServerScript]) -> Result<(), Error> {
        let local = match source {
            Some(source) => LocalScripts::load(self.run.store(), source)?,
            None => LocalScripts::default(),
        };
        *self.run.summary().plan(ObjectType::SieveScript) = Plan::compute(server, &local).counts();
        Ok(())
    }

    fn apply(&mut self, source: SourceId, server: &[ServerScript]) -> Result<(), Error> {
        let logger = self.run.options().logger;
        let local = LocalScripts::load(self.run.store(), source)?;
        let mut active: Option<LocalId> = None;
        for action in Plan::compute(server, &local).actions {
            match action {
                Action::New(script) => match self.write(source, |writer| writer.insert(script))? {
                    Ok(local) => {
                        if script.active {
                            active = Some(local);
                        }
                        self.counts.created += 1;
                        self.counts.fetched += 1;
                    }
                    Err(e) => {
                        logger.warn(format_args!(
                            "cannot store sieve script \"{}\": {e}",
                            script.name
                        ));
                        self.counts.failed += 1;
                    }
                },
                Action::Present {
                    script,
                    local,
                    stored,
                    outcome,
                } => {
                    match outcome {
                        PresentOutcome::Unchanged | PresentOutcome::ActiveOnly => {
                            self.counts.skipped += 1;
                        }
                        PresentOutcome::ContentUpdated => {
                            match self.write(source, |writer| {
                                writer.update_content(local, stored, script)
                            })? {
                                Ok(()) => self.counts.fetched += 1,
                                Err(e) => {
                                    logger.warn(format_args!(
                                        "cannot update sieve script \"{}\": {e}",
                                        script.name
                                    ));
                                    self.counts.failed += 1;
                                }
                            }
                        }
                    }
                    if script.active {
                        active = Some(local);
                    }
                }
                Action::Vanished { name, local } => {
                    match self.write(source, |writer| writer.delete(name, local))? {
                        Ok(()) => self.counts.deleted += 1,
                        Err(e) => {
                            logger.warn(format_args!("cannot delete sieve script \"{name}\": {e}"));
                            self.counts.failed += 1;
                        }
                    }
                }
            }
        }
        self.write(source, |writer| writer.assign_active(active))?
    }

    fn write<T>(
        &mut self,
        source: SourceId,
        op: impl FnOnce(ScriptWriter<'_>) -> Result<T, Error>,
    ) -> Result<Result<T, Error>, Error> {
        let tx = self.run.archive().transaction()?;
        let written = op(ScriptWriter {
            store: tx.store(),
            source,
        });
        if written.is_ok() {
            tx.commit()?;
        }
        Ok(written)
    }

    fn finish(self, result: Result<(), Error>) -> RunOutcome {
        let SieveRun {
            mut run,
            mut control,
            counts,
        } = self;
        *run.counts(ObjectType::SieveScript) = counts;
        let stats = control.stats();
        if let Err(error) = result {
            return run.abort(error, stats);
        }
        control.logout();
        run.finish(stats, GcFailure::Abort)
    }
}
