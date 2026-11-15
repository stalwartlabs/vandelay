/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use crate::imap::client::ImapClient;
use crate::imap::command::{Command, UidSet};
use crate::imap::error::ImapError;
use crate::imap::response::Untagged;
use crate::imap::retry::Disposition;
use crate::imap::session::ImapSession;
use crate::net::backoff::BackoffState;

use super::fetch::FetchAttrs;

pub const MAX_IMAP_CONNECTIONS: usize = 8;

const FETCH_ITEMS: &[&str] = &["UID", "FLAGS", "INTERNALDATE", "RFC822.SIZE", "BODY.PEEK[]"];

pub struct FetchJob {
    pub folder: Arc<str>,
    pub wire_name: Arc<str>,
    pub uids: Vec<u32>,
}

pub enum FetchEvent {
    Item(FetchAttrs),
    ChunkDone {
        folder: Arc<str>,
        requested: usize,
        outcome: Result<(), ImapError>,
    },
}

pub struct WorkerArgs {
    pub session: Arc<ImapSession>,
    pub max_retries: u32,
    pub backoff: BackoffState,
}

pub struct WorkerPool {
    job_tx: Sender<FetchJob>,
    event_rx: Receiver<FetchEvent>,
    handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    pub fn start(args: WorkerArgs, pool_size: usize) -> WorkerPool {
        let size = pool_size.clamp(1, MAX_IMAP_CONNECTIONS);
        let (job_tx, job_rx) = unbounded::<FetchJob>();
        let (event_tx, event_rx) = unbounded::<FetchEvent>();
        let args = Arc::new(args);
        let handles = (0..size)
            .map(|_| {
                let mut worker = Worker {
                    args: args.clone(),
                    client: None,
                    selected: None,
                };
                let job_rx = job_rx.clone();
                let event_tx = event_tx.clone();
                thread::spawn(move || worker.run(&job_rx, &event_tx))
            })
            .collect();
        WorkerPool {
            job_tx,
            event_rx,
            handles,
        }
    }

    pub fn submit(&self, job: FetchJob) {
        let _ = self.job_tx.send(job);
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<FetchEvent, RecvTimeoutError> {
        self.event_rx.recv_timeout(timeout)
    }

    pub fn shutdown(self) {
        drop(self.job_tx);
        for h in self.handles {
            let _ = h.join();
        }
    }
}

struct Worker {
    args: Arc<WorkerArgs>,
    client: Option<ImapClient>,
    selected: Option<Arc<str>>,
}

impl Worker {
    fn run(&mut self, job_rx: &Receiver<FetchJob>, event_tx: &Sender<FetchEvent>) {
        while let Ok(job) = job_rx.recv() {
            let outcome =
                match catch_unwind(AssertUnwindSafe(|| self.run_with_retry(&job, event_tx))) {
                    Ok(r) => r,
                    Err(_) => {
                        self.disconnect();
                        Err(ImapError::Protocol("worker thread panicked".into()))
                    }
                };
            let _ = event_tx.send(FetchEvent::ChunkDone {
                folder: job.folder,
                requested: job.uids.len(),
                outcome,
            });
        }
    }

    fn disconnect(&mut self) {
        self.client = None;
        self.selected = None;
    }

    fn run_with_retry(
        &mut self,
        job: &FetchJob,
        event_tx: &Sender<FetchEvent>,
    ) -> Result<(), ImapError> {
        let args = self.args.clone();
        let mut transient_attempts: u32 = 0;
        let mut transport_attempts: u32 = 0;
        loop {
            if self.client.is_none() {
                match args.session.reopen() {
                    Ok(c) => {
                        self.client = Some(c);
                        self.selected = None;
                    }
                    Err(e) => {
                        if e.disposition() == Disposition::TransportDrop
                            && transport_attempts < args.max_retries
                        {
                            transport_attempts += 1;
                            thread::sleep(args.backoff.transport_delay(transport_attempts));
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            let Some(client) = self.client.as_mut() else {
                return Err(ImapError::Protocol(
                    "worker pool: client slot empty after connect".into(),
                ));
            };
            match Worker::fetch(client, &mut self.selected, job, event_tx) {
                Ok(()) => {
                    args.backoff.reset();
                    return Ok(());
                }
                Err(e) => match e.disposition() {
                    Disposition::TransportDrop => {
                        self.disconnect();
                        if transport_attempts >= args.max_retries {
                            return Err(e);
                        }
                        transport_attempts += 1;
                        thread::sleep(args.backoff.transport_delay(transport_attempts));
                    }
                    Disposition::Transient => {
                        if transient_attempts >= args.max_retries {
                            return Err(e);
                        }
                        transient_attempts += 1;
                        thread::sleep(args.backoff.next_shared_delay());
                    }
                    _ => return Err(e),
                },
            }
        }
    }

    fn fetch(
        client: &mut ImapClient,
        selected: &mut Option<Arc<str>>,
        job: &FetchJob,
        event_tx: &Sender<FetchEvent>,
    ) -> Result<(), ImapError> {
        if selected.as_deref() != Some(&*job.folder) {
            client.run_collect(&Command::Select(&job.wire_name))?;
            *selected = Some(job.folder.clone());
        }
        let set = UidSet::from_sorted(&job.uids);
        client.run_streamed(
            &Command::UidFetch {
                set: &set,
                items: FETCH_ITEMS,
            },
            |u| {
                if let Untagged::Fetch { .. } = &u
                    && let Some(attrs) = FetchAttrs::from_untagged(u)
                {
                    let _ = event_tx.send(FetchEvent::Item(attrs));
                }
            },
        )?;
        Ok(())
    }
}
