/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write as _;

use crate::archive::ids::ews::EwsIds;
use crate::archive::sources::{SourceChange, SourceKey, SourceKind};
use crate::archive::{Archive, LocalId, SourceId, Store};
use crate::error::Error;
use crate::exchange::ews::EwsClient;
use crate::exchange::ews::autodiscover::{Autodiscover, DiscoveryResult};
use crate::exchange::ews::oauth::{AcquiredToken, OAuthFlow, REFRESH_SCHEDULE};
use crate::exchange::ews::types::MailboxKind;
use crate::exchange::oauth::{RefreshPlan, TokenRefresher};
use crate::logging::Logger;
use crate::net::auth::Auth;
use crate::net::http::{ClientOptions, RetryStats};
use crate::sync::run::{GcFailure, ImportRun, SourceClaim, SourceLabel};
use crate::sync::vanish::{Children, OnRowError, VanishedSweep};
use crate::sync::{RunOptions, RunOutcome, Summary, TypeCounts};
use crate::types::ObjectType;

use super::folder_sync::ItemKind;
use super::folders::FolderPlan;
use super::items::{ItemEnumeration, ItemRunCtx, SyncBatch};

const LABELS: [ObjectType; 6] = [
    ObjectType::Mailbox,
    ObjectType::Calendar,
    ObjectType::AddressBook,
    ObjectType::Email,
    ObjectType::CalendarEvent,
    ObjectType::ContactCard,
];
const DRY_RUN_LABELS: [ObjectType; 3] = [
    ObjectType::Mailbox,
    ObjectType::Calendar,
    ObjectType::AddressBook,
];
const DEFAULT_PATH: &str = "/EWS/Exchange.asmx";
const ENDPOINT: &str = "Exchange.asmx";
const EWS_SEGMENT: &str = "EWS";
const BASIC_AUTH_BLOCKED_HOSTS: [&str; 4] = [
    "outlook.office365.com",
    "outlook.office.com",
    "outlook.office365.us",
    "office365.us",
];

#[derive(Debug, Clone)]
pub enum EwsAuth {
    Basic { user: String, password: String },
    Bearer { token: String },
    OAuth(OAuthFlow),
}

#[derive(Debug, Clone)]
pub struct EwsImport {
    pub url: Option<String>,
    pub mailbox: Option<String>,
    pub mailbox_kind: MailboxKind,
    pub auth: EwsAuth,
    pub ews_connections: usize,
    pub getitem_batch: usize,
    pub attachment_batch: usize,
    pub enumeration: ItemEnumeration,
    pub source_change: SourceChange,
}

pub struct EwsCoordinator<'a> {
    pub store: Store<'a>,
    pub source: SourceId,
    pub items: ItemRunCtx<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUrl(String);

#[derive(Default)]
struct EwsCounts([TypeCounts; LABELS.len()]);

struct Session {
    auth: Auth,
    acquired: Option<AcquiredToken>,
    mailbox: String,
    url: SessionUrl,
}

impl<'a> EwsCoordinator<'a> {
    pub fn ids(&self) -> EwsIds<'a> {
        self.store.ews_ids(self.source)
    }

    pub fn ids_in<'s>(&self, store: Store<'s>) -> EwsIds<'s> {
        store.ews_ids(self.source)
    }

    pub fn sweep<K: AsRef<str>>(
        &self,
        ty: ObjectType,
        vanished: &[(K, LocalId)],
        counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        VanishedSweep::new(self.store, OnRowError::Count, Children::Restrict)
            .run(
                ty,
                vanished,
                counts,
                |store, item_id, _| self.ids_in(store).delete(ty, item_id.as_ref()),
                |_, _, _| {},
            )
            .map_err(Error::partial)
    }
}

impl EwsCounts {
    fn split(&mut self, kind: ItemKind) -> (&mut TypeCounts, &mut TypeCounts) {
        let [mailbox, calendar, addressbook, email, event, contact] = &mut self.0;
        match kind {
            ItemKind::Message => (mailbox, email),
            ItemKind::Contact => (addressbook, contact),
            ItemKind::Event => (calendar, event),
        }
    }

    fn store_into(self, summary: &mut Summary) {
        for (label, counts) in LABELS.into_iter().zip(self.0) {
            *summary.counts(label) = counts;
        }
    }
}

impl SessionUrl {
    pub fn normalise(raw: &str) -> SessionUrl {
        let Ok(parsed) = url::Url::parse(raw) else {
            return SessionUrl(raw.to_owned());
        };
        let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        let mut out = String::with_capacity(raw.len());
        let _ = write!(out, "{}://{host}", parsed.scheme());
        if let Some(port) = parsed.port() {
            let _ = write!(out, ":{port}");
        }
        let path = parsed.path();
        match path.rsplit_once('/') {
            None | Some(("", "")) => out.push_str(DEFAULT_PATH),
            Some((dir, last)) if last.eq_ignore_ascii_case(ENDPOINT) => {
                match dir.rsplit_once('/') {
                    Some((head, segment)) if segment.eq_ignore_ascii_case(EWS_SEGMENT) => {
                        let _ = write!(out, "{head}/{EWS_SEGMENT}/{ENDPOINT}");
                    }
                    _ => {
                        let _ = write!(out, "{dir}/{ENDPOINT}");
                    }
                }
            }
            Some(_) => out.push_str(path),
        }
        SessionUrl(out)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl EwsAuth {
    fn resolve(&self, options: ClientOptions) -> Result<(Auth, Option<AcquiredToken>), Error> {
        let acquired = match self {
            EwsAuth::Basic { user, password } => {
                return Ok((
                    Auth::Basic {
                        user: user.clone(),
                        password: password.clone(),
                    },
                    None,
                ));
            }
            EwsAuth::Bearer { token } => AcquiredToken::from_bearer(token.clone()),
            EwsAuth::OAuth(flow) => flow.acquire(options)?,
        };
        Ok((
            Auth::Bearer {
                token: acquired.access_token.clone(),
            },
            Some(acquired),
        ))
    }

    fn check_basic_policy(&self, url: &str) -> Result<(), Error> {
        let EwsAuth::Basic { .. } = self else {
            return Ok(());
        };
        let parsed = url::Url::parse(url)
            .map_err(|e| Error::Usage(format!("invalid EWS URL {url:?}: {e}")))?;
        if parsed.scheme() != "https" {
            return Err(Error::Connection(
                "EWS requires https:// (basic auth refused on cleartext)".to_owned(),
            ));
        }
        let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        if BASIC_AUTH_BLOCKED_HOSTS
            .iter()
            .any(|blocked| host.ends_with(blocked))
        {
            return Err(Error::Connection(
                "Basic auth is disabled on Exchange Online; use --auth-bearer".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Session {
    fn username<'s>(&'s self, auth: &'s EwsAuth) -> &'s str {
        match auth {
            EwsAuth::Basic { user, .. } => user,
            EwsAuth::Bearer { .. } | EwsAuth::OAuth(_) => self
                .acquired
                .as_ref()
                .and_then(|a| a.upn.as_deref())
                .unwrap_or(&self.mailbox),
        }
    }
}

impl EwsImport {
    pub fn run(self, options: RunOptions) -> RunOutcome {
        let prepared = Archive::open(&options.archive)
            .map_err(Error::from)
            .and_then(|archive| self.connect(&options).map(|session| (archive, session)));
        match prepared {
            Ok((archive, session)) => self.run_session(archive, session, options),
            Err(error) => RunOutcome::failed(error),
        }
    }

    fn run_session(&self, archive: Archive, session: Session, options: RunOptions) -> RunOutcome {
        let logger = options.logger;
        let account_id = self.mailbox_kind.account_id(&session.mailbox).to_string();
        let key = SourceKey {
            kind: SourceKind::ExchangeEws,
            url: session.url.as_str(),
            account_id: &account_id,
        };
        let label = SourceLabel {
            name: session.acquired.as_ref().and_then(|a| a.name.as_deref()),
            username: session.username(&self.auth),
        };
        let client_options = options.client_options();
        let mut run = match ImportRun::start_on(archive, options, &key, label, self.source_change) {
            Ok(run) => run,
            Err(error) => return RunOutcome::failed(error),
        };

        let Session {
            auth,
            acquired,
            mailbox,
            url,
        } = session;
        let client = EwsClient::new(auth, client_options);
        let owner = (self.mailbox_kind != MailboxKind::PublicFolders).then_some(mailbox.as_str());
        client.set_anchor_mailbox(owner.map(str::to_owned));
        if let EwsAuth::OAuth(OAuthFlow::ClientCredentials { .. }) = &self.auth {
            client.set_impersonation(Some(mailbox.clone()));
        }
        let _refresher = self.spawn_refresher(&client, acquired.as_ref(), client_options, logger);
        let items = ItemRunCtx {
            client: &client,
            url: url.as_str(),
            owner_smtp: owner,
            batch_size: self.getitem_batch.max(1),
            attachment_batch: self.attachment_batch.max(1),
            connections: self.ews_connections,
            enumeration: self.enumeration,
            sync_batch: SyncBatch::MAX,
            logger,
        };

        let source = match run.claim() {
            SourceClaim::Registered(source) => *source,
            SourceClaim::DryRun(_) => {
                return match self.dry_run(&items) {
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
        let ctx = EwsCoordinator {
            store: run.store(),
            source,
            items,
        };
        let mut counts = EwsCounts::default();
        let result = self.reconcile(&ctx, &mut counts);
        counts.store_into(run.summary());
        let stats = client.stats();
        match result {
            Ok(()) => run.finish(stats, GcFailure::Warn),
            Err(error) => run.abort(error, stats),
        }
    }

    fn reconcile(&self, ctx: &EwsCoordinator<'_>, counts: &mut EwsCounts) -> Result<(), Error> {
        let items = &ctx.items;
        let plan = FolderPlan::discover(
            items.client,
            items.url,
            self.mailbox_kind,
            self.mailbox.as_deref(),
            items.logger,
        )?;
        for kind in [ItemKind::Message, ItemKind::Event, ItemKind::Contact] {
            let (folders, members) = counts.split(kind);
            ctx.reconcile_folders(kind, &plan, folders, members)?;
        }
        for kind in ItemKind::ALL {
            ctx.reconcile_items(kind, &plan, counts.split(kind).1)?;
        }
        Ok(())
    }

    fn dry_run(&self, items: &ItemRunCtx<'_>) -> Result<[u64; 3], Error> {
        let plan = FolderPlan::discover(
            items.client,
            items.url,
            self.mailbox_kind,
            self.mailbox.as_deref(),
            items.logger,
        )?;
        Ok([plan.mail.len(), plan.calendar.len(), plan.contacts.len()].map(|n| n as u64))
    }

    fn connect(&self, options: &RunOptions) -> Result<Session, Error> {
        let (auth, acquired) = self.auth.resolve(options.client_options())?;
        let discovery = self.autodiscover(acquired.as_ref(), options.client_options())?;
        options.logger.progress(format_args!(
            "EWS endpoint {} ({})",
            discovery.ews_url, discovery.source
        ));
        let mailbox = self.resolve_mailbox(acquired.as_ref())?;
        let url = SessionUrl::normalise(&discovery.ews_url);
        self.auth.check_basic_policy(url.as_str())?;
        Ok(Session {
            auth,
            acquired,
            mailbox,
            url,
        })
    }

    fn autodiscover(
        &self,
        acquired: Option<&AcquiredToken>,
        options: ClientOptions,
    ) -> Result<DiscoveryResult, Error> {
        let email = self
            .mailbox
            .as_deref()
            .or_else(|| acquired.and_then(|a| a.upn.as_deref()));
        Ok(Autodiscover::new(options).discover(self.url.as_deref(), email, None)?)
    }

    fn resolve_mailbox(&self, acquired: Option<&AcquiredToken>) -> Result<String, Error> {
        if let Some(mailbox) = &self.mailbox {
            return Ok(mailbox.clone());
        }
        if let Some(upn) = acquired.and_then(|a| a.upn.as_ref()) {
            return Ok(upn.clone());
        }
        if let EwsAuth::Basic { user, .. } = &self.auth
            && user.contains('@')
        {
            return Ok(user.clone());
        }
        Err(Error::Account(
            "could not resolve mailbox SMTP; pass --mailbox".to_owned(),
        ))
    }

    fn spawn_refresher(
        &self,
        client: &EwsClient,
        initial: Option<&AcquiredToken>,
        options: ClientOptions,
        logger: Logger,
    ) -> Option<TokenRefresher> {
        let EwsAuth::OAuth(flow) = &self.auth else {
            return None;
        };
        let initial = initial?;
        let deadline_unix = initial.deadline_unix(TokenRefresher::unix_now())?;
        let flow = flow.clone();
        let client = client.clone();
        TokenRefresher::spawn(
            RefreshPlan {
                label: "EWS",
                schedule: REFRESH_SCHEDULE,
                deadline_unix,
                refresh_token: initial.refresh_token.clone(),
                logger,
            },
            move |refresh_token| flow.renew(options, refresh_token),
            move |token| {
                client.set_auth(Auth::Bearer {
                    token: token.access_token.clone(),
                })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::logging::Level;
    use crate::net::http::TlsPolicy;
    use crate::sync::{RunMode, SummaryType};

    const NS: &str = " xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                      xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
                      xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\"";

    fn soap(server: &mut mockito::Server, action: &str, inner: &str) -> mockito::Mock {
        server
            .mock("POST", "/EWS/Exchange.asmx")
            .match_header(
                "soapaction",
                mockito::Matcher::Regex(format!("/{action}\"$")),
            )
            .with_status(200)
            .with_header("content-type", "text/xml; charset=utf-8")
            .with_body(format!(
                "<soap:Envelope{NS}><soap:Body>{inner}</soap:Body></soap:Envelope>"
            ))
            .create()
    }

    fn basic(user: &str) -> EwsAuth {
        EwsAuth::Basic {
            user: user.to_owned(),
            password: "p".to_owned(),
        }
    }

    #[test]
    fn basic_against_office365_endpoint_is_refused() {
        let res =
            basic("alice@x").check_basic_policy("https://outlook.office365.com/EWS/Exchange.asmx");
        assert!(matches!(res, Err(Error::Connection(_))));
    }

    #[test]
    fn basic_over_https_on_prem_is_allowed() {
        assert!(
            basic("alice@x")
                .check_basic_policy("https://exchange.example.com/EWS/Exchange.asmx")
                .is_ok()
        );
    }

    #[test]
    fn basic_over_http_is_refused() {
        assert!(
            basic("alice")
                .check_basic_policy("http://exchange.example.com/EWS/Exchange.asmx")
                .is_err()
        );
    }

    #[test]
    fn normalise_ews_url_lowercases_host_and_keeps_canonical_path() {
        let normalise = |raw| SessionUrl::normalise(raw).0;
        assert_eq!(
            normalise("https://OUTLOOK.OFFICE365.COM/EWS/Exchange.asmx"),
            "https://outlook.office365.com/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise("https://outlook.office365.com:443/ews/exchange.asmx"),
            "https://outlook.office365.com/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise("https://srv.example.com:8443/EWS/Exchange.asmx"),
            "https://srv.example.com:8443/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise("https://srv.example.com"),
            "https://srv.example.com/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise("https://srv.example.com/"),
            "https://srv.example.com/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise("https://srv.example.com/Exchange.ASMX"),
            "https://srv.example.com/Exchange.asmx"
        );
        assert_eq!(
            normalise("https://srv.example.com/a/ews/EXCHANGE.asmx"),
            "https://srv.example.com/a/EWS/Exchange.asmx"
        );
        assert_eq!(
            normalise("https://srv.example.com/owa/x"),
            "https://srv.example.com/owa/x"
        );
        assert_eq!(normalise("not a url"), "not a url");
    }

    // A SQL failure on calendars after the mailboxes keeps the mailbox counts (R15).
    #[test]
    fn abort_after_mailboxes_keeps_the_mailbox_counts() {
        let mut server = mockito::Server::new();
        let _find = soap(
            &mut server,
            "FindFolder",
            "<m:FindFolderResponse><m:ResponseMessages>\
             <m:FindFolderResponseMessage ResponseClass=\"Success\"><m:ResponseCode>NoError</m:ResponseCode>\
             <m:RootFolder TotalItemsInView=\"2\" IncludesLastItemInRange=\"true\"><t:Folders>\
             <t:Folder><t:FolderId Id=\"INBOX\" ChangeKey=\"K\"/><t:ParentFolderId Id=\"ROOT\"/>\
             <t:DisplayName>Inbox</t:DisplayName><t:FolderClass>IPF.Note</t:FolderClass></t:Folder>\
             <t:CalendarFolder><t:FolderId Id=\"CAL\" ChangeKey=\"K\"/><t:ParentFolderId Id=\"ROOT\"/>\
             <t:DisplayName>Calendar</t:DisplayName><t:FolderClass>IPF.Appointment</t:FolderClass></t:CalendarFolder>\
             </t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>",
        );
        let _get = soap(
            &mut server,
            "GetFolder",
            "<m:GetFolderResponse><m:ResponseMessages>\
             <m:GetFolderResponseMessage ResponseClass=\"Success\"><m:ResponseCode>NoError</m:ResponseCode>\
             <m:Folders><t:Folder><t:FolderId Id=\"ROOT\" ChangeKey=\"K\"/></t:Folder></m:Folders>\
             </m:GetFolderResponseMessage></m:ResponseMessages></m:GetFolderResponse>",
        );
        let archive = Archive::open_in_memory().unwrap();
        archive
            .connection()
            .execute_batch(
                "CREATE TEMP TRIGGER no_calendars BEFORE INSERT ON calendars \
                 BEGIN SELECT RAISE(ABORT, 'calendars refused'); END",
            )
            .unwrap();
        let import = EwsImport {
            url: None,
            mailbox: Some("alice@x.com".to_owned()),
            mailbox_kind: MailboxKind::Primary,
            auth: EwsAuth::Bearer {
                token: "t".to_owned(),
            },
            ews_connections: 1,
            getitem_batch: 10,
            attachment_batch: 10,
            enumeration: ItemEnumeration::FindItem,
            source_change: SourceChange::Reject,
        };
        let session = Session {
            auth: Auth::Bearer {
                token: "t".to_owned(),
            },
            acquired: None,
            mailbox: "alice@x.com".to_owned(),
            url: SessionUrl(format!("{}/EWS/Exchange.asmx", server.url())),
        };
        let options = RunOptions {
            archive: PathBuf::new(),
            threads: 1,
            mode: RunMode::Apply,
            max_retries: 0,
            tls: TlsPolicy::Verify,
            logger: Logger::new(Level::Quiet),
        };
        let outcome = import.run_session(archive, session, options);
        assert!(
            matches!(outcome.error, Some(Error::Partial(_))),
            "the calendar insert must abort the run: {:?}",
            outcome.error
        );
        let summary = &outcome.summary;
        assert_eq!(summary.get(ObjectType::Mailbox).map(|c| c.created), Some(1));
        assert_eq!(
            summary.get(ObjectType::Calendar).map(|c| c.created),
            Some(0)
        );
        assert!(
            summary
                .per_type
                .iter()
                .map(|(t, _)| *t)
                .eq(LABELS.map(SummaryType::from))
        );
    }
}
