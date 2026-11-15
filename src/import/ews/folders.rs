/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::{HashMap, HashSet};

use crate::archive::ids::ews::FolderRow;
use crate::archive::rows::{AddressBookRecord, CalendarRecord, MailboxRecord, Subscribed};
use crate::archive::{LocalId, Store};
use crate::console::Counted;
use crate::error::Error;
use crate::exchange::ews::EwsClient;
use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{
    FindFolderResponse, FindItemResponse, FolderEntry, ResponseMessage,
};
use crate::exchange::ews::types::{DistinguishedFolderId, FolderClass, MailboxKind};
use crate::exchange::ews::xml::{EwsRequest, FolderRef, FolderShape, Traversal};
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::tree::{DanglingParent, ParentMap};

use super::coordinator::EwsCoordinator;
use super::folder_sync::ItemKind;

const CLASS_SAMPLE_SIZE: u32 = 10;
const AVAILABILITY_ALL: &str = "all";

const WELL_KNOWN_ROLES: &[(DistinguishedFolderId, Option<&str>)] = &[
    (DistinguishedFolderId::Inbox, Some("inbox")),
    (DistinguishedFolderId::SentItems, Some("sent")),
    (DistinguishedFolderId::Drafts, Some("drafts")),
    (DistinguishedFolderId::DeletedItems, Some("trash")),
    (DistinguishedFolderId::JunkEmail, Some("junk")),
    (DistinguishedFolderId::Archive, Some("archive")),
    (DistinguishedFolderId::Outbox, None),
    (DistinguishedFolderId::ConversationHistory, None),
];

pub struct FolderPlan {
    pub mail: Vec<ClassifiedFolder>,
    pub calendar: Vec<ClassifiedFolder>,
    pub contacts: Vec<ClassifiedFolder>,
    pub well_known_roles: HashMap<String, &'static str>,
    pub root_folder_id: String,
}

#[derive(Debug, Clone)]
pub struct ClassifiedFolder {
    pub folder: FolderEntry,
    pub kind: FolderClass,
}

struct Discovery<'a> {
    client: &'a EwsClient,
    url: &'a str,
    mailbox_kind: MailboxKind,
    mailbox: Option<&'a str>,
    logger: Logger,
}

enum FolderRecord<'r> {
    Mailbox(MailboxRecord<'r>),
    Calendar(CalendarRecord<'r>),
    AddressBook(AddressBookRecord<'r>),
}

impl FolderPlan {
    pub fn discover(
        client: &EwsClient,
        url: &str,
        mailbox_kind: MailboxKind,
        mailbox: Option<&str>,
        logger: Logger,
    ) -> Result<FolderPlan, EwsError> {
        Discovery {
            client,
            url,
            mailbox_kind,
            mailbox: mailbox.filter(|_| mailbox_kind != MailboxKind::PublicFolders),
            logger,
        }
        .plan()
    }

    fn drop_hidden(mut folders: Vec<FolderEntry>) -> Vec<FolderEntry> {
        if !folders.iter().any(|f| f.is_hidden) {
            return folders;
        }
        let by_id: HashMap<&str, &FolderEntry> = folders
            .iter()
            .map(|f| (f.folder_id.id.as_str(), f))
            .collect();
        let keep: Vec<bool> = folders
            .iter()
            .map(|f| {
                let mut current = Some(f);
                for _ in 0..=by_id.len() {
                    let Some(folder) = current else {
                        return true;
                    };
                    if folder.is_hidden {
                        return false;
                    }
                    current = folder
                        .parent_id
                        .as_deref()
                        .and_then(|p| by_id.get(p).copied());
                }
                true
            })
            .collect();
        let mut keep = keep.into_iter();
        folders.retain(|_| keep.next().unwrap_or(true));
        folders
    }

    fn reparent_under_mail(mail: &mut [ClassifiedFolder], parents: &HashMap<String, String>) {
        let nearest: Vec<Option<String>> = {
            let mail_ids: HashSet<&str> = mail
                .iter()
                .map(|f| f.folder.folder_id.id.as_str())
                .collect();
            mail.iter()
                .map(|folder| {
                    let mut parent = folder.folder.parent_id.as_deref();
                    for _ in 0..=parents.len() {
                        match parent {
                            Some(p) if !mail_ids.contains(p) => match parents.get(p) {
                                Some(grandparent) => parent = Some(grandparent),
                                None => break,
                            },
                            _ => break,
                        }
                    }
                    parent.map(str::to_owned)
                })
                .collect()
        };
        for (folder, parent) in mail.iter_mut().zip(nearest) {
            folder.folder.parent_id = parent;
        }
    }

    fn parents_first(folders: &[ClassifiedFolder]) -> Vec<&ClassifiedFolder> {
        let mut tree = ParentMap::with_capacity(DanglingParent::Ignored, folders.len());
        tree.extend(folders.iter().map(|f| {
            (
                f.folder.folder_id.id.as_str(),
                f.folder.parent_id.as_deref(),
            )
        }));
        let mut ordered: Vec<&ClassifiedFolder> = folders.iter().collect();
        ordered.sort_by_cached_key(|f| tree.depth(&f.folder.folder_id.id.as_str()));
        ordered
    }

    fn parent_of(
        &self,
        folder: &ClassifiedFolder,
        placed: &HashMap<&str, LocalId>,
    ) -> Option<LocalId> {
        let parent = folder.folder.parent_id.as_deref()?;
        if parent == self.root_folder_id || parent.is_empty() {
            return None;
        }
        placed.get(parent).copied()
    }
}

impl Discovery<'_> {
    fn plan(&self) -> Result<FolderPlan, EwsError> {
        let request = EwsRequest::FindFolder {
            parent: FolderRef::distinguished(self.mailbox_kind.distinguished_root(), self.mailbox),
            traversal: Traversal::Deep,
        };
        let resp = self.client.call(self.url, &request)?;
        let parsed = FindFolderResponse::parse(&resp.body)?;
        self.logger.progress(format_args!(
            "FindFolder returned {}",
            Counted::new(parsed.folders.len() as u64, "folder", "folders")
        ));
        let folders = FolderPlan::drop_hidden(parsed.folders);
        let parents: HashMap<String, String> = folders
            .iter()
            .filter_map(|f| Some((f.folder_id.id.clone(), f.parent_id.clone()?)))
            .collect();
        let mut mail = Vec::new();
        let mut calendar = Vec::new();
        let mut contacts = Vec::new();
        for folder in folders {
            let mut class = FolderClass::from_ipf(&folder.folder_class);
            if class == FolderClass::Mail && FolderClass::is_mail_fallback(&folder.folder_class) {
                let classless = folder.folder_class.trim().is_empty();
                if classless && self.mailbox_kind != MailboxKind::Archive {
                    class = self.sample_kind(&folder);
                }
                let kind = match class {
                    FolderClass::Calendar => "calendar",
                    FolderClass::Contacts => "contacts",
                    FolderClass::Mail | FolderClass::Skipped => "mail",
                };
                let name = &folder.display_name;
                if classless {
                    self.logger.note(format_args!(
                        "folder \"{name}\" has no FolderClass; importing it as {kind}"
                    ));
                } else {
                    self.logger.note(format_args!(
                        "folder \"{name}\" has an unrecognized FolderClass \"{}\"; importing it as {kind}",
                        folder.folder_class
                    ));
                }
            }
            let bucket = match (self.mailbox_kind, class) {
                (_, FolderClass::Mail) => &mut mail,
                (MailboxKind::Archive, _) | (_, FolderClass::Skipped) => continue,
                (_, FolderClass::Calendar) => &mut calendar,
                (_, FolderClass::Contacts) => &mut contacts,
            };
            bucket.push(ClassifiedFolder {
                folder,
                kind: class,
            });
        }
        FolderPlan::reparent_under_mail(&mut mail, &parents);
        Ok(FolderPlan {
            mail,
            calendar,
            contacts,
            well_known_roles: self.well_known_roles()?,
            root_folder_id: self.root_folder_id()?,
        })
    }

    fn sample_kind(&self, folder: &FolderEntry) -> FolderClass {
        let request = EwsRequest::FindItem {
            parent: FolderRef::Concrete(&folder.folder_id),
            traversal: Traversal::Shallow,
            offset: 0,
            page_size: CLASS_SAMPLE_SIZE,
        };
        let sampled = self
            .client
            .call(self.url, &request)
            .and_then(|resp| FindItemResponse::parse(&resp.body));
        match sampled {
            Ok(found) => FolderClass::from_item_classes(
                found
                    .items
                    .iter()
                    .filter_map(|item| item.item_class.as_deref()),
            ),
            Err(e) => {
                self.logger.progress(format_args!(
                    "FindItem sample of \"{}\" failed: {e}; treating it as mail",
                    folder.display_name
                ));
                FolderClass::Mail
            }
        }
    }

    fn well_known_roles(&self) -> Result<HashMap<String, &'static str>, EwsError> {
        if self.mailbox_kind != MailboxKind::Primary {
            return Ok(HashMap::new());
        }
        let folders: Vec<FolderRef<'_>> = WELL_KNOWN_ROLES
            .iter()
            .map(|(d, _)| FolderRef::distinguished(*d, self.mailbox))
            .collect();
        let request = EwsRequest::GetFolder {
            folders: &folders,
            shape: FolderShape::IdOnly,
        };
        let resp = match self.client.call(self.url, &request) {
            Ok(r) => r,
            Err(e) => {
                self.logger.progress(format_args!(
                    "GetFolder for distinguished ids failed: {e}; roles will be empty"
                ));
                return Ok(HashMap::new());
            }
        };
        let messages =
            ResponseMessage::parse_all(&resp.body, "GetFolderResponseMessage").unwrap_or_default();
        let mut map = HashMap::new();
        for (msg, (_, role)) in messages.into_iter().zip(WELL_KNOWN_ROLES) {
            if !msg.success {
                continue;
            }
            let Ok(Some(entry)) = FolderEntry::parse_inner(&msg.inner_xml) else {
                continue;
            };
            if let Some(role) = role
                && !entry.folder_id.id.is_empty()
            {
                map.insert(entry.folder_id.id, *role);
            }
        }
        Ok(map)
    }

    fn root_folder_id(&self) -> Result<String, EwsError> {
        let request = EwsRequest::GetFolder {
            folders: &[FolderRef::distinguished(
                self.mailbox_kind.distinguished_root(),
                self.mailbox,
            )],
            shape: FolderShape::IdOnly,
        };
        let resp = self.client.call(self.url, &request)?;
        Ok(FolderEntry::parse_get_response(&resp.body)?
            .into_iter()
            .next()
            .map(|e| e.folder_id.id)
            .unwrap_or_default())
    }
}

impl<'r> FolderRecord<'r> {
    fn new(
        kind: ItemKind,
        name: &'r str,
        parent: Option<LocalId>,
        role: Option<&'r str>,
    ) -> FolderRecord<'r> {
        match kind {
            ItemKind::Message => FolderRecord::Mailbox(MailboxRecord {
                name,
                parent,
                role,
                sort_order: 0,
                subscribed: Subscribed::Yes,
            }),
            ItemKind::Event => FolderRecord::Calendar(CalendarRecord {
                name,
                description: None,
                color: None,
                sort_order: 0,
                subscribed: Subscribed::Yes,
                is_visible: true,
                is_default: false,
                include_in_availability: AVAILABILITY_ALL,
                default_alerts_with_time: None,
                default_alerts_without_time: None,
                time_zone: None,
            }),
            ItemKind::Contact => FolderRecord::AddressBook(AddressBookRecord {
                name,
                description: None,
                sort_order: 0,
                is_default: false,
                subscribed: Subscribed::Yes,
            }),
        }
    }

    fn insert(&self, store: Store<'_>) -> rusqlite::Result<LocalId> {
        match self {
            FolderRecord::Mailbox(r) => store.mailboxes().insert(r),
            FolderRecord::Calendar(r) => store.calendars().insert(r),
            FolderRecord::AddressBook(r) => store.address_books().insert(r),
        }
    }

    fn update(&self, store: Store<'_>, id: LocalId) -> rusqlite::Result<()> {
        match self {
            FolderRecord::Mailbox(r) => store.mailboxes().update(id, r),
            FolderRecord::Calendar(r) => store.calendars().update(id, r),
            FolderRecord::AddressBook(r) => store.address_books().update(id, r),
        }
        .map(drop)
    }
}

impl EwsCoordinator<'_> {
    pub fn reconcile_folders(
        &self,
        kind: ItemKind,
        plan: &FolderPlan,
        folder_counts: &mut TypeCounts,
        item_counts: &mut TypeCounts,
    ) -> Result<(), Error> {
        let container = kind.container();
        let ids = self.ids();
        let local = ids.all(container).map_err(Error::partial)?;
        let folders = FolderPlan::parents_first(kind.folders(plan));
        let mut placed: HashMap<&str, LocalId> = local
            .iter()
            .map(|(id, row)| (id.as_str(), row.local_id))
            .collect();
        for folder in &folders {
            let id = folder.folder.folder_id.id.as_str();
            let (parent, role) = match kind {
                ItemKind::Message => (
                    plan.parent_of(folder, &placed),
                    plan.well_known_roles.get(id).copied(),
                ),
                ItemKind::Contact | ItemKind::Event => (None, None),
            };
            let record = FolderRecord::new(kind, &folder.folder.display_name, parent, role);
            let local_id =
                self.upsert_folder(kind, folder, local.get(id), &record, folder_counts)?;
            placed.insert(id, local_id);
        }

        let on_server: HashSet<&str> = folders
            .iter()
            .map(|f| f.folder.folder_id.id.as_str())
            .collect();
        let mut vanished: Vec<(&str, LocalId)> = local
            .iter()
            .filter(|(id, _)| !on_server.contains(id.as_str()))
            .map(|(id, row)| (id.as_str(), row.local_id))
            .collect();
        if vanished.is_empty() {
            return Ok(());
        }
        let mut tree = ParentMap::with_capacity(DanglingParent::Ignored, local.len());
        tree.extend(
            local
                .values()
                .map(|row| (row.item_id.as_str(), Some(row.folder_id.as_str()))),
        );
        tree.children_first(&mut vanished, |(id, _)| id);
        let mut children = Vec::new();
        for (folder_id, _) in &vanished {
            children.extend(
                ids.items_in(kind.item(), folder_id)
                    .map_err(Error::partial)?
                    .into_iter()
                    .map(|row| (row.item_id, row.local_id)),
            );
        }
        self.sweep(kind.item(), &children, item_counts)?;
        self.sweep(container, &vanished, folder_counts)
    }

    fn upsert_folder(
        &self,
        kind: ItemKind,
        folder: &ClassifiedFolder,
        existing: Option<&FolderRow>,
        record: &FolderRecord<'_>,
        counts: &mut TypeCounts,
    ) -> Result<LocalId, Error> {
        let container = kind.container();
        let entry = &folder.folder;
        let tx = self.store.begin().map_err(Error::partial)?;
        let store = tx.store();
        let ids = self.ids_in(store);
        let local_id = match existing {
            Some(row) => {
                record.update(store, row.local_id).map_err(Error::partial)?;
                ids.update_change_key(container, &entry.folder_id.id, &entry.folder_id.change_key)
                    .map_err(Error::partial)?;
                counts.fetched += 1;
                row.local_id
            }
            None => {
                let id = record.insert(store).map_err(Error::partial)?;
                ids.insert(
                    container,
                    entry.parent_id.as_deref().unwrap_or(""),
                    &entry.folder_id.id,
                    &entry.folder_id.change_key,
                    id,
                )
                .map_err(Error::partial)?;
                counts.created += 1;
                id
            }
        };
        tx.commit().map_err(Error::partial)?;
        Ok(local_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::rows::EmailRecord;
    use crate::archive::sources::{SourceKey, SourceKind};
    use crate::archive::{Archive, SourceId};
    use crate::exchange::ews::types::FolderId;
    use crate::import::ews::items::{ItemEnumeration, ItemRunCtx, SyncBatch};
    use crate::logging::Level;
    use crate::net::auth::Auth;
    use crate::net::http::{ClientOptions, TlsPolicy};
    use crate::net::retry::RetryPolicy;
    use crate::types::ObjectType;

    fn client() -> EwsClient {
        EwsClient::new(
            Auth::Bearer {
                token: "t".to_owned(),
            },
            ClientOptions {
                retry: RetryPolicy::new(0),
                tls: TlsPolicy::Verify,
                logger: Logger::new(Level::Quiet),
            },
        )
    }

    fn source(archive: &Archive) -> SourceId {
        archive
            .store()
            .sources()
            .upsert(
                &SourceKey {
                    kind: SourceKind::ExchangeEws,
                    url: "https://x/EWS/Exchange.asmx",
                    account_id: "u@d",
                },
                Some("u"),
                "u@d",
            )
            .unwrap()
    }

    fn coordinator<'a>(archive: &'a Archive, client: &'a EwsClient) -> EwsCoordinator<'a> {
        EwsCoordinator {
            store: archive.store(),
            source: source(archive),
            items: ItemRunCtx {
                client,
                url: "https://x/EWS/Exchange.asmx",
                owner_smtp: None,
                batch_size: 10,
                attachment_batch: 10,
                connections: 1,
                enumeration: ItemEnumeration::FindItem,
                sync_batch: SyncBatch::MAX,
                logger: Logger::new(Level::Quiet),
            },
        }
    }

    fn entry(id: &str, parent: &str, class: &str, hidden: bool) -> FolderEntry {
        FolderEntry {
            folder_id: FolderId {
                id: id.to_owned(),
                change_key: "CK".to_owned(),
            },
            parent_id: Some(parent.to_owned()),
            display_name: id.to_owned(),
            folder_class: class.to_owned(),
            is_hidden: hidden,
            ..FolderEntry::default()
        }
    }

    fn classified(id: &str, parent: &str, kind: FolderClass) -> ClassifiedFolder {
        ClassifiedFolder {
            folder: entry(id, parent, "", false),
            kind,
        }
    }

    fn plan(mail: Vec<ClassifiedFolder>, calendar: Vec<ClassifiedFolder>) -> FolderPlan {
        FolderPlan {
            mail,
            calendar,
            contacts: Vec::new(),
            well_known_roles: HashMap::new(),
            root_folder_id: "ROOT".to_owned(),
        }
    }

    fn count(archive: &Archive, table: &str) -> i64 {
        archive
            .connection()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn reconcile(ctx: &EwsCoordinator<'_>, plan: &FolderPlan) -> [TypeCounts; 2] {
        let mut counts: [TypeCounts; 2] = Default::default();
        let [folders, items] = &mut counts;
        for kind in [ItemKind::Message, ItemKind::Event] {
            ctx.reconcile_folders(kind, plan, folders, items).unwrap();
        }
        counts
    }

    // A vanished folder takes its archived items with it, id rows included.
    #[test]
    fn vanished_folder_cascades_deletion_of_its_child_items() {
        let archive = Archive::open_in_memory().unwrap();
        let client = client();
        let ctx = coordinator(&archive, &client);
        reconcile(
            &ctx,
            &plan(
                vec![classified("FOLDER", "ROOT", FolderClass::Mail)],
                vec![],
            ),
        );
        let store = archive.store();
        let blob = store.blobs().intern(b"m").unwrap();
        let folder = ctx
            .ids()
            .local_id(ObjectType::Mailbox, "FOLDER")
            .unwrap()
            .unwrap();
        let email = store
            .emails()
            .insert(&EmailRecord {
                blob,
                received_at: "",
                mailboxes: &[folder],
                keywords: "[]",
                message_match: None,
            })
            .unwrap();
        ctx.ids()
            .insert(ObjectType::Email, "FOLDER", "MAIL", "CK", email)
            .unwrap();

        let [folders, items] = reconcile(&ctx, &plan(vec![], vec![]));
        assert_eq!(folders.deleted, 1);
        assert_eq!(items.deleted, 1, "child email must be cascade-deleted");
        assert_eq!(count(&archive, "mailboxes"), 0);
        assert_eq!(count(&archive, "emails"), 0);
        assert_eq!(count(&archive, "sync_id_exchange_ews"), 0);
    }

    // A vanished parent and child are both removed, the child first.
    #[test]
    fn vanished_subtree_is_deleted_leaf_first() {
        let archive = Archive::open_in_memory().unwrap();
        let client = client();
        let ctx = coordinator(&archive, &client);
        reconcile(
            &ctx,
            &plan(
                vec![
                    classified("CHILD", "PARENT", FolderClass::Mail),
                    classified("PARENT", "ROOT", FolderClass::Mail),
                    classified("GRAND", "CHILD", FolderClass::Mail),
                ],
                vec![],
            ),
        );
        let parent_of_child: Option<LocalId> = archive
            .connection()
            .query_row(
                "SELECT parent_id FROM mailboxes WHERE name = 'CHILD'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(parent_of_child.is_some(), "parents are created first");
        let [folders, _] = reconcile(&ctx, &plan(vec![], vec![]));
        assert_eq!((folders.deleted, folders.failed), (3, 0));
        assert_eq!(count(&archive, "mailboxes"), 0);
    }

    #[test]
    fn hidden_folders_are_dropped_with_their_descendants() {
        let folders = vec![
            entry("CONTACTS", "ROOT", "IPF.Contact", false),
            entry("RECIPIENT_CACHE", "CONTACTS", "IPF.Contact", true),
            entry("YAMMER", "ROOT", "IPF.Note", true),
            entry("YAMMER_FEEDS", "YAMMER", "IPF.Note", false),
            entry("YAMMER_DEEP", "YAMMER_FEEDS", "IPF.Note", false),
            entry("INBOX", "ROOT", "IPF.Note", false),
        ];
        let kept: Vec<String> = FolderPlan::drop_hidden(folders)
            .into_iter()
            .map(|f| f.folder_id.id)
            .collect();
        assert_eq!(kept, ["CONTACTS", "INBOX"]);
    }

    #[test]
    fn mail_folder_is_filed_under_its_nearest_mail_ancestor() {
        let parents: HashMap<String, String> = [
            ("PROJECTS", "ROOT"),
            ("PCAL", "PROJECTS"),
            ("SUB", "PCAL"),
            ("CAL", "ROOT"),
            ("NOTES", "CAL"),
            ("INBOX", "ROOT"),
            ("A", "B"),
            ("B", "A"),
            ("LOOP", "A"),
        ]
        .into_iter()
        .map(|(id, parent)| (id.to_owned(), parent.to_owned()))
        .collect();
        let mut mail = vec![
            classified("PROJECTS", "ROOT", FolderClass::Mail),
            classified("SUB", "PCAL", FolderClass::Mail),
            classified("NOTES", "CAL", FolderClass::Mail),
            classified("INBOX", "ROOT", FolderClass::Mail),
            classified("LOOP", "A", FolderClass::Mail),
        ];
        FolderPlan::reparent_under_mail(&mut mail, &parents);
        let parent_of = |id: &str| {
            mail.iter()
                .find(|f| f.folder.folder_id.id == id)
                .and_then(|f| f.folder.parent_id.clone())
        };
        assert_eq!(parent_of("SUB").as_deref(), Some("PROJECTS"));
        assert_eq!(parent_of("NOTES").as_deref(), Some("ROOT"));
        assert_eq!(parent_of("INBOX").as_deref(), Some("ROOT"));
        assert_ne!(parent_of("LOOP").as_deref(), Some("PROJECTS"));
    }

    #[test]
    fn classless_folder_kind_comes_from_its_items() {
        let mut server = mockito::Server::new();
        let url = format!("{}/EWS/Exchange.asmx", server.url());
        let _m = server
            .mock("POST", "/EWS/Exchange.asmx")
            .with_status(200)
            .with_header("content-type", "text/xml; charset=utf-8")
            .with_body(
                "<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                 xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
                 xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\"><soap:Body>\
                 <m:FindItemResponse><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
                 <m:ResponseCode>NoError</m:ResponseCode>\
                 <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\"><t:Items>\
                 <t:Contact><t:ItemId Id=\"C1\"/><t:ItemClass>IPM.Contact</t:ItemClass></t:Contact>\
                 </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages>\
                 </m:FindItemResponse></soap:Body></soap:Envelope>",
            )
            .create();
        let client = client();
        let discovery = Discovery {
            client: &client,
            url: &url,
            mailbox_kind: MailboxKind::Primary,
            mailbox: None,
            logger: Logger::new(Level::Default),
        };
        assert_eq!(
            discovery.sample_kind(&entry("LOCAL", "ROOT", "", false)),
            FolderClass::Contacts
        );
    }

    #[test]
    fn rerun_does_not_take_a_calendar_local_id_as_mailbox_parent() {
        let archive = Archive::open_in_memory().unwrap();
        let client = client();
        let ctx = coordinator(&archive, &client);
        let plan = plan(
            vec![
                classified("INBOX", "ROOT", FolderClass::Mail),
                classified("NOTES", "CAL", FolderClass::Mail),
            ],
            vec![
                classified("CAL", "ROOT", FolderClass::Calendar),
                classified("CAL2", "ROOT", FolderClass::Calendar),
            ],
        );
        for _ in 0..2 {
            reconcile(&ctx, &plan);
            let parent: Option<i64> = archive
                .connection()
                .query_row(
                    "SELECT parent_id FROM mailboxes WHERE name = 'NOTES'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(parent, None);
        }
    }
}
