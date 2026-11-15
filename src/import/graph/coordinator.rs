/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::mem;

use serde_json::Value;

use crate::archive::ids::graph::GraphIds;
use crate::archive::sources::{SourceChange, SourceKey, SourceKind};
use crate::archive::{Archive, LocalId, SourceId, Store};
use crate::error::Error;
use crate::exchange::graph::ResolvedPrincipal;
use crate::exchange::graph::api::Endpoints;
use crate::exchange::graph::client::GraphClient;
use crate::exchange::graph::error::GraphError;
use crate::exchange::graph::oauth::{REFRESH_ATTEMPTS, REFRESH_SCHEDULE, SCOPES};
use crate::exchange::graph::types::{EventBodyFormat, MailboxKind, Surfaces};
use crate::exchange::oauth::{
    AcquiredToken, Authority, Dialect, OAuthClient, OAuthError, RefreshPlan, TokenRefresher,
};
use crate::logging::Logger;
use crate::net::http::{ClientOptions, RetryStats};
use crate::sync::run::{GcFailure, ImportRun, SourceClaim, SourceLabel};
use crate::sync::tree::{DanglingParent, ParentMap};
use crate::sync::vanish::{Children, OnRowError, RowFailure, VanishedSweep};
use crate::sync::{RunOptions, RunOutcome, Summary, TypeCounts};
use crate::types::ObjectType;

#[derive(Debug, Clone)]
pub enum GraphAuth {
    PreAcquired {
        token: String,
    },
    DeviceCode {
        authority: String,
        client_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct GraphImport {
    pub auth: GraphAuth,
    pub api_base: String,
    pub user_target: Option<String>,
    pub mailbox_kind: MailboxKind,
    pub surfaces: Surfaces,
    pub event_body_format: EventBodyFormat,
    pub graph_connections: usize,
    pub top: usize,
    pub exception_window_years: i32,
    pub contact_photos: ContactPhotos,
    pub event_attachments: EventAttachments,
    pub source_change: SourceChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactPhotos {
    Fetch,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAttachments {
    Fetch,
    Skip,
}

pub const CHUNK_SIZE: usize = 100;
pub const MAX_GRAPH_CONNECTIONS: usize = 16;
pub const MAX_PAGE_SIZE: usize = 1000;
const LABELS: [ObjectType; 7] = [
    ObjectType::Mailbox,
    ObjectType::Email,
    ObjectType::Calendar,
    ObjectType::CalendarEvent,
    ObjectType::AddressBook,
    ObjectType::ContactCard,
    ObjectType::FileNode,
];
const DRY_RUN_LABELS: [ObjectType; 3] = [
    ObjectType::Mailbox,
    ObjectType::Calendar,
    ObjectType::AddressBook,
];

pub struct GraphCoordinator<'a> {
    pub store: Store<'a>,
    pub client: &'a GraphClient,
    pub endpoints: &'a Endpoints,
    pub source: SourceId,
    pub top: usize,
    pub workers: usize,
    pub logger: Logger,
    pub event_body_format: EventBodyFormat,
    pub exception_window_years: i32,
    pub contact_photos: ContactPhotos,
    pub event_attachments: EventAttachments,
}

impl<'a> GraphCoordinator<'a> {
    pub fn ids(&self) -> GraphIds<'a> {
        self.store.graph_ids(self.source)
    }

    pub fn ids_in<'s>(&self, store: Store<'s>) -> GraphIds<'s> {
        store.graph_ids(self.source)
    }

    pub fn known(&self, ty: ObjectType) -> Result<HashMap<String, LocalId>, Error> {
        Ok(self.ids().all(ty)?)
    }

    pub fn sweep(
        &self,
        ty: ObjectType,
        vanished: &[(String, LocalId)],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let noun = ty.noun();
        let hint = match ty {
            ObjectType::Mailbox => " (live children?)",
            _ => "",
        };
        let logger = self.logger;
        VanishedSweep::new(self.store, OnRowError::Count, Children::Restrict).run(
            ty,
            vanished,
            counts,
            |store, graph_id, _| self.ids_in(store).delete(ty, graph_id),
            |graph_id, local_id, failure| match failure {
                RowFailure::Sql(e) => logger.warn(format_args!(
                    "{noun} {graph_id} (local id {local_id}) could not be deleted{hint}: {e}"
                )),
                RowFailure::LiveChildren => logger.warn(format_args!(
                    "{noun} {graph_id} (local id {local_id}) could not be deleted{hint}"
                )),
            },
        )?;
        Ok(())
    }

    pub fn parents(&self, ty: ObjectType) -> Result<ParentMap<LocalId>, Error> {
        let mut stmt = self
            .store
            .connection()
            .prepare(&format!("SELECT id, parent_id FROM {}", ty.table()))?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut parents = ParentMap::new(DanglingParent::Counts);
        for row in rows {
            let (id, parent) = row?;
            parents.insert(id, parent);
        }
        Ok(parents)
    }
}

#[derive(Clone, Copy)]
struct UserTarget<'a>(&'a str);

impl UserTarget<'_> {
    fn is_uuid(self) -> bool {
        self.0.len() == 36
            && self.0.chars().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit(),
            })
    }
}

#[derive(Default)]
struct SurfaceCounts([TypeCounts; LABELS.len()]);

impl SurfaceCounts {
    fn store_into(self, summary: &mut Summary) {
        for (label, counts) in LABELS.into_iter().zip(self.0) {
            *summary.counts(label) = counts;
        }
    }
}

struct Session {
    client: GraphClient,
    endpoints: Endpoints,
    principal: ResolvedPrincipal,
    acquired: AcquiredToken,
}

impl GraphImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let logger = options.logger;
        let prepared = Archive::open(&options.archive)
            .map_err(Error::from)
            .and_then(|archive| self.connect(&options).map(|session| (archive, session)));
        let (archive, session) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return RunOutcome::failed(error),
        };
        let session_url = self.session_url();
        let account_id = self
            .mailbox_kind
            .account_id(&session.principal.id)
            .to_string();
        let key = SourceKey {
            kind: SourceKind::ExchangeGraph,
            url: &session_url,
            account_id: &account_id,
        };
        let label = SourceLabel {
            name: Some(&session.principal.user_principal_name),
            username: &session.principal.user_principal_name,
        };
        let refresh_options = options.client_options();
        let mut run = match ImportRun::start_on(archive, options, &key, label, self.source_change) {
            Ok(run) => run,
            Err(error) => return RunOutcome::failed(error),
        };

        if self.mailbox_kind == MailboxKind::Archive {
            logger.warn(format_args!(
                "--mailbox-kind archive: Microsoft Online Archive holds only mail; \
                 calendar and contact surfaces are skipped for this run"
            ));
        }

        let source = match run.claim() {
            SourceClaim::Registered(source) => *source,
            SourceClaim::DryRun(_) => {
                return match self.dry_run(&session) {
                    Ok(found) => {
                        for (ty, found) in DRY_RUN_LABELS.into_iter().zip(found) {
                            run.counts(ty).created = found;
                            run.summary().plan(ty).new = found;
                        }
                        run.finish(RetryStats::default(), GcFailure::Warn)
                    }
                    Err(error) => run.abort(error, RetryStats::default()),
                };
            }
        };
        let _refresher =
            self.spawn_refresher(&session.client, &session.acquired, refresh_options, logger);
        let ctx = GraphCoordinator {
            store: run.store(),
            client: &session.client,
            endpoints: &session.endpoints,
            source,
            top: self.top.clamp(1, MAX_PAGE_SIZE),
            workers: self.graph_connections.clamp(1, MAX_GRAPH_CONNECTIONS),
            logger,
            event_body_format: self.event_body_format,
            exception_window_years: self.exception_window_years,
            contact_photos: self.contact_photos,
            event_attachments: self.event_attachments,
        };
        let mut counts = SurfaceCounts::default();
        let result = self.reconcile(&ctx, &mut counts);
        counts.store_into(run.summary());
        let stats = session.client.stats();
        match result {
            Ok(()) => run.finish(stats, GcFailure::Warn),
            Err(error) => run.abort(error, stats),
        }
    }

    fn reconcile(
        &self,
        ctx: &GraphCoordinator<'_>,
        counts: &mut SurfaceCounts,
    ) -> Result<(), Error> {
        let primary = self.mailbox_kind != MailboxKind::Archive;
        let [mailboxes, emails, calendars, events, books, contacts, files] = &mut counts.0;
        if self.surfaces.mail {
            let folders = ctx.reconcile_mail_folders(self.mailbox_kind, mailboxes)?;
            ctx.reconcile_messages(&folders, emails)?;
        }
        if self.surfaces.calendar && primary {
            let found = ctx.reconcile_calendars(calendars)?;
            ctx.reconcile_events(&found, events)?;
        }
        if self.surfaces.contacts && primary {
            let found = ctx.reconcile_address_books(books)?;
            ctx.reconcile_contacts(&found, contacts)?;
        }
        if self.surfaces.files && primary {
            ctx.reconcile_files(files)?;
        }
        Ok(())
    }

    fn dry_run(&self, session: &Session) -> Result<[u64; 3], Error> {
        let client = &session.client;
        let endpoints = &session.endpoints;
        let mut found = [0u64; 3];
        let [mailboxes, calendars, books] = &mut found;
        *mailboxes =
            GraphImport::enumerate_mail_folders(client, endpoints, self.mailbox_kind, self.top)?
                .len() as u64;
        if self.mailbox_kind != MailboxKind::Archive {
            *calendars = client
                .collect_all_values(&endpoints.calendars(self.top), &[])?
                .len() as u64;
            *books = client
                .collect_all_values(&endpoints.contact_folders(self.top), &[])?
                .len() as u64;
        }
        Ok(found)
    }

    pub fn enumerate_mail_folders(
        client: &GraphClient,
        endpoints: &Endpoints,
        mailbox_kind: MailboxKind,
        top: usize,
    ) -> Result<Vec<Value>, GraphError> {
        let mut all = Vec::new();
        let mut frontier: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut visit = |level: Vec<Value>, frontier: &mut Vec<String>| {
            for folder in level {
                let Some(id) = folder.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if seen.insert(id.to_owned()) {
                    frontier.push(id.to_owned());
                    all.push(folder);
                }
            }
        };
        let initial = endpoints.mail_folders_root(mailbox_kind, top);
        visit(client.collect_all_values(&initial, &[])?, &mut frontier);
        while let Some(parent) = frontier.pop() {
            let url = endpoints.mail_folder_child_folders(&parent, top);
            visit(client.collect_all_values(&url, &[])?, &mut frontier);
        }
        Ok(all)
    }

    fn connect(&self, options: &RunOptions) -> Result<Session, Error> {
        let acquired = self.acquire(options)?;
        let client = GraphClient::new(acquired.access_token.clone(), options.client_options());
        let endpoints = self.endpoints(&client)?;
        let principal = ResolvedPrincipal::resolve(&client, &endpoints, acquired.upn.as_deref())?;
        Ok(Session {
            client,
            endpoints,
            principal,
            acquired,
        })
    }

    fn acquire(&self, options: &RunOptions) -> Result<AcquiredToken, Error> {
        match &self.auth {
            GraphAuth::PreAcquired { token } => Ok(AcquiredToken::from_bearer(token.clone())),
            GraphAuth::DeviceCode {
                authority,
                client_id,
            } => {
                let authority = Authority::new(authority.clone());
                OAuthClient::new(
                    options.client_options(),
                    &authority,
                    client_id,
                    SCOPES,
                    Dialect::Graph,
                )
                .device_code()
                .map_err(|e| Error::from(GraphError::from(e)))
            }
        }
    }

    fn endpoints(&self, client: &GraphClient) -> Result<Endpoints, Error> {
        let Some(target) = self.user_target.as_deref() else {
            return Ok(Endpoints::for_me(&self.api_base));
        };
        let resolved = if UserTarget(target).is_uuid() {
            target.to_owned()
        } else {
            self.resolve_user_id(client, target)
                .map_err(|e| Error::Connection(format!("user resolution: {e}")))?
        };
        Ok(Endpoints::for_user(&self.api_base, &resolved))
    }

    fn resolve_user_id(&self, client: &GraphClient, upn_or_id: &str) -> Result<String, GraphError> {
        let url = format!(
            "{}/users/{}?$select=id,userPrincipalName",
            self.api_base.trim_end_matches('/'),
            upn_or_id
        );
        let mut body = client.get_json(&url)?;
        match body.get_mut("id") {
            Some(Value::String(id)) => Ok(mem::take(id)),
            _ => Err(GraphError::Malformed(
                "user resolution missing id".to_owned(),
            )),
        }
    }

    fn session_url(&self) -> String {
        let authority = match &self.auth {
            GraphAuth::DeviceCode { authority, .. } => Cow::Borrowed(authority.as_str()),
            GraphAuth::PreAcquired { .. } => {
                Cow::Owned(Authority::for_tenant("common").into_string())
            }
        };
        format!(
            "{}|{}",
            authority.trim_end_matches('/'),
            self.api_base.trim_end_matches('/')
        )
    }

    fn spawn_refresher(
        &self,
        client: &GraphClient,
        initial: &AcquiredToken,
        options: ClientOptions,
        logger: Logger,
    ) -> Option<TokenRefresher> {
        let GraphAuth::DeviceCode {
            authority,
            client_id,
        } = &self.auth
        else {
            return None;
        };
        let refresh_token = initial.refresh_token.clone()?;
        let deadline_unix = initial.deadline_unix(TokenRefresher::unix_now())?;
        let authority = Authority::new(authority.clone());
        let client_id = client_id.clone();
        let client = client.clone();
        TokenRefresher::spawn(
            RefreshPlan {
                label: "graph",
                schedule: REFRESH_SCHEDULE,
                deadline_unix,
                refresh_token: Some(refresh_token),
                logger,
            },
            move |refresh_token| {
                let refresh_token = refresh_token.unwrap_or_default();
                OAuthClient::new(options, &authority, &client_id, SCOPES, Dialect::Graph)
                    .refresh(refresh_token, REFRESH_ATTEMPTS)
                    .map_err(|e| OAuthError(GraphError::from(e).to_string()))
            },
            move |token| client.set_bearer(token.access_token.clone()),
        )
    }
}

impl ResolvedPrincipal {
    fn resolve(
        client: &GraphClient,
        endpoints: &Endpoints,
        fallback_upn: Option<&str>,
    ) -> Result<ResolvedPrincipal, Error> {
        let mut value = client.get_json(&endpoints.me_select_id_upn())?;
        let id = match value.get_mut("id") {
            Some(Value::String(id)) => mem::take(id),
            _ => return Err(Error::Connection("graph principal has no id".to_owned())),
        };
        let user_principal_name = match value.get_mut("userPrincipalName") {
            Some(Value::String(upn)) => mem::take(upn),
            _ => fallback_upn.map(str::to_owned).unwrap_or_default(),
        };
        Ok(ResolvedPrincipal {
            id,
            user_principal_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_session_url_concatenates_authority_and_base() {
        let config = GraphImport {
            auth: GraphAuth::DeviceCode {
                authority: "https://login.microsoftonline.com/common".to_owned(),
                client_id: "uuid".to_owned(),
            },
            api_base: "https://graph.microsoft.com/v1.0".to_owned(),
            user_target: None,
            mailbox_kind: MailboxKind::Primary,
            surfaces: Surfaces::ALL,
            event_body_format: EventBodyFormat::Text,
            graph_connections: 4,
            top: 100,
            exception_window_years: 5,
            contact_photos: ContactPhotos::Fetch,
            event_attachments: EventAttachments::Fetch,
            source_change: SourceChange::Reject,
        };
        let url = config.session_url();
        assert_eq!(
            url,
            "https://login.microsoftonline.com/common|https://graph.microsoft.com/v1.0"
        );
    }

    #[test]
    fn looks_like_uuid_only_matches_well_formed() {
        assert!(UserTarget("12345678-1234-1234-1234-123456789abc").is_uuid());
        assert!(!UserTarget("alice@x.com").is_uuid());
        assert!(!UserTarget("12345678123412341234123456789abc").is_uuid());
    }
}
