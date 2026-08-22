/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::imap::client::{ConnectMode, ImapClient};
use crate::imap::command;
use crate::imap::error::ImapError;
use crate::imap::name::{alternate_mailbox_name, encode_mailbox_name_with};
use crate::imap::response::Untagged;
use crate::imap::retry::{BackoffState, Disposition, RetryPolicy, classify};
use crate::imap::transport::Connector;
use crate::logging::Logger;

use super::coordinator::{Endpoint, ImapAuth, authenticate_client};
use super::fetch::FetchAttrs;

pub const HARD_CAP: usize = 8;

pub struct FetchJob {
    pub folder: String,
    pub uidvalidity: u32,
    pub uids: Vec<u32>,
}

pub enum FetchEvent {
    Item {
        folder: String,
        uidvalidity: u32,
        attrs: FetchAttrs,
    },
    ChunkDone {
        folder: String,
        uidvalidity: u32,
        uids_requested: Vec<u32>,
        outcome: Result<(), ImapError>,
    },
}

pub struct WorkerArgs {
    pub connector: Arc<Connector>,
    pub endpoint: Arc<Endpoint>,
    pub mode: ConnectMode,
    pub auth: ImapAuth,
    pub compress: bool,
    pub policy: RetryPolicy,
    pub backoff: BackoffState,
    pub logger: Logger,
}

pub struct WorkerPool {
    job_tx: Sender<FetchJob>,
    event_rx: Receiver<FetchEvent>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl WorkerPool {
    pub fn start(args: WorkerArgs, pool_size: usize) -> Result<WorkerPool, ImapError> {
        let size = pool_size.clamp(1, HARD_CAP);
        let (job_tx, job_rx) = unbounded::<FetchJob>();
        let (event_tx, event_rx) = unbounded::<FetchEvent>();
        let mut handles = Vec::with_capacity(size);
        let args = Arc::new(args);

        for _ in 0..size {
            let args = args.clone();
            let job_rx = job_rx.clone();
            let event_tx = event_tx.clone();
            let handle = thread::spawn(move || {
                worker_loop(args, job_rx, event_tx);
            });
            handles.push(handle);
        }

        Ok(WorkerPool {
            job_tx,
            event_rx,
            handles,
        })
    }

    pub fn submit(&self, job: FetchJob) {
        let _ = self.job_tx.send(job);
    }

    pub fn recv(&self) -> Result<FetchEvent, crossbeam_channel::RecvError> {
        self.event_rx.recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<FetchEvent, crossbeam_channel::RecvTimeoutError> {
        self.event_rx.recv_timeout(timeout)
    }

    pub fn shutdown(self) {
        drop(self.job_tx);
        for h in self.handles {
            let _ = h.join();
        }
    }
}

fn worker_loop(args: Arc<WorkerArgs>, job_rx: Receiver<FetchJob>, event_tx: Sender<FetchEvent>) {
    let mut client: Option<ImapClient> = None;
    let mut current_folder: Option<String> = None;
    while let Ok(job) = job_rx.recv() {
        let job_folder = job.folder.clone();
        let job_uv = job.uidvalidity;
        let job_uids = job.uids.clone();
        let event_tx_for_job = event_tx.clone();
        let outcome = match catch_unwind(AssertUnwindSafe(|| {
            run_job_with_retry(
                &args,
                &mut client,
                &mut current_folder,
                &job,
                &event_tx_for_job,
            )
        })) {
            Ok(r) => r,
            Err(_) => {
                client = None;
                current_folder = None;
                Err(ImapError::Protocol("worker thread panicked".into()))
            }
        };
        let _ = event_tx.send(FetchEvent::ChunkDone {
            folder: job_folder,
            uidvalidity: job_uv,
            uids_requested: job_uids,
            outcome,
        });
    }
}

fn run_job_with_retry(
    args: &WorkerArgs,
    client_slot: &mut Option<ImapClient>,
    current_folder: &mut Option<String>,
    job: &FetchJob,
    event_tx: &Sender<FetchEvent>,
) -> Result<(), ImapError> {
    let mut transient_attempts: u32 = 0;
    let mut transport_attempts: u32 = 0;
    loop {
        if client_slot.is_none() {
            match connect_and_auth(args) {
                Ok(c) => {
                    *client_slot = Some(c);
                    *current_folder = None;
                }
                Err(e) => {
                    let disp = classify(&e);
                    if disp == Disposition::TransportDrop
                        && transport_attempts < args.policy.max_retries
                    {
                        transport_attempts += 1;
                        std::thread::sleep(args.backoff.transport_delay(transport_attempts));
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        let Some(client) = client_slot.as_mut() else {
            return Err(ImapError::Protocol(
                "worker pool: client slot empty after connect".into(),
            ));
        };
        match run_one_job(client, current_folder, job, event_tx) {
            Ok(()) => {
                args.backoff.reset();
                return Ok(());
            }
            Err(e) => match classify(&e) {
                Disposition::TransportDrop => {
                    *client_slot = None;
                    *current_folder = None;
                    if transport_attempts >= args.policy.max_retries {
                        return Err(e);
                    }
                    transport_attempts += 1;
                    std::thread::sleep(args.backoff.transport_delay(transport_attempts));
                }
                Disposition::Transient => {
                    if transient_attempts >= args.policy.max_retries {
                        return Err(e);
                    }
                    transient_attempts += 1;
                    std::thread::sleep(args.backoff.next_shared_delay());
                }
                _ => return Err(e),
            },
        }
    }
}

fn connect_and_auth(args: &WorkerArgs) -> Result<ImapClient, ImapError> {
    let mut client = ImapClient::connect(
        &args.connector,
        &args.endpoint.host,
        args.endpoint.port,
        args.mode,
        args.logger,
    )?;
    authenticate_client(&mut client, &args.auth)
        .map_err(|e| ImapError::AuthFailed(e.to_string()))?;
    let _ = client.refresh_capabilities();
    if args.compress && client.has_capability("COMPRESS=DEFLATE") {
        client.compress_deflate()?;
    }
    if client.has_capability("ENABLE") && client.has_capability("UTF8=ACCEPT") {
        let _ = client.enable(&["UTF8=ACCEPT"]);
    }
    Ok(client)
}

fn run_one_job(
    client: &mut ImapClient,
    current_folder: &mut Option<String>,
    job: &FetchJob,
    event_tx: &Sender<FetchEvent>,
) -> Result<(), ImapError> {
    if current_folder.as_deref() != Some(job.folder.as_str()) {
        let utf8_accept = client.utf8_accept();
        let wire = encode_mailbox_name_with(&job.folder, utf8_accept);
        if let Err(e) = client.run_collect(&command::select(&wire)) {
            match alternate_mailbox_name(&job.folder, utf8_accept) {
                Some(alternate) if matches!(e, ImapError::No(_)) => {
                    client.run_collect(&command::select(&alternate))?;
                }
                _ => return Err(e),
            }
        }
        *current_folder = Some(job.folder.clone());
    }
    let set = command::format_uid_set(&job.uids, true);
    let folder = job.folder.clone();
    let uv = job.uidvalidity;
    client.run_streamed(
        &command::uid_fetch(
            &set,
            &["UID", "FLAGS", "INTERNALDATE", "RFC822.SIZE", "BODY.PEEK[]"],
        ),
        |u| {
            if let Untagged::Fetch { .. } = &u
                && let Some(attrs) = super::fetch::extract(&u)
            {
                let _ = event_tx.send(FetchEvent::Item {
                    folder: folder.clone(),
                    uidvalidity: uv,
                    attrs,
                });
            }
        },
    )?;
    Ok(())
}
