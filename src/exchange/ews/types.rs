/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::iter;

pub use crate::exchange::mailbox::MailboxKind;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ItemId {
    pub id: String,
    pub change_key: String,
}

impl ItemId {
    pub fn new(id: impl Into<String>, change_key: impl Into<String>) -> ItemId {
        ItemId {
            id: id.into(),
            change_key: change_key.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FolderId {
    pub id: String,
    pub change_key: String,
}

impl FolderId {
    pub fn new(id: impl Into<String>, change_key: impl Into<String>) -> FolderId {
        FolderId {
            id: id.into(),
            change_key: change_key.into(),
        }
    }
}

impl MailboxKind {
    pub fn distinguished_root(self) -> DistinguishedFolderId {
        match self {
            MailboxKind::Primary => DistinguishedFolderId::MsgFolderRoot,
            MailboxKind::Archive => DistinguishedFolderId::ArchiveRoot,
            MailboxKind::PublicFolders => DistinguishedFolderId::PublicFoldersRoot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistinguishedFolderId {
    MsgFolderRoot,
    ArchiveRoot,
    PublicFoldersRoot,
    Inbox,
    SentItems,
    Drafts,
    DeletedItems,
    JunkEmail,
    Outbox,
    ConversationHistory,
    Archive,
}

impl DistinguishedFolderId {
    pub fn as_str(self) -> &'static str {
        match self {
            DistinguishedFolderId::MsgFolderRoot => "msgfolderroot",
            DistinguishedFolderId::ArchiveRoot => "archiveroot",
            DistinguishedFolderId::PublicFoldersRoot => "publicfoldersroot",
            DistinguishedFolderId::Inbox => "inbox",
            DistinguishedFolderId::SentItems => "sentitems",
            DistinguishedFolderId::Drafts => "drafts",
            DistinguishedFolderId::DeletedItems => "deleteditems",
            DistinguishedFolderId::JunkEmail => "junkemail",
            DistinguishedFolderId::Outbox => "outbox",
            DistinguishedFolderId::ConversationHistory => "conversationhistory",
            DistinguishedFolderId::Archive => "archive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ServerVersion {
    Exchange2007,
    Exchange2010,
    Exchange2010Sp1,
    Exchange2010Sp2,
    Exchange2013,
    Exchange2013Sp1,
    Exchange2016,
}

impl ServerVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerVersion::Exchange2007 => "Exchange2007",
            ServerVersion::Exchange2010 => "Exchange2010",
            ServerVersion::Exchange2010Sp1 => "Exchange2010_SP1",
            ServerVersion::Exchange2010Sp2 => "Exchange2010_SP2",
            ServerVersion::Exchange2013 => "Exchange2013",
            ServerVersion::Exchange2013Sp1 => "Exchange2013_SP1",
            ServerVersion::Exchange2016 => "Exchange2016",
        }
    }

    pub fn from_build(major: u32, minor: u32) -> ServerVersion {
        match (major, minor) {
            (8, _) => ServerVersion::Exchange2007,
            (14, 0) => ServerVersion::Exchange2010,
            (14, 1) => ServerVersion::Exchange2010Sp1,
            (14, 2 | 3) => ServerVersion::Exchange2010Sp2,
            (15, 0) => ServerVersion::Exchange2013,
            _ => ServerVersion::Exchange2016,
        }
    }

    pub fn lower(self) -> Option<ServerVersion> {
        match self {
            ServerVersion::Exchange2016 => Some(ServerVersion::Exchange2013Sp1),
            ServerVersion::Exchange2013Sp1 => Some(ServerVersion::Exchange2013),
            ServerVersion::Exchange2013 => Some(ServerVersion::Exchange2010Sp2),
            ServerVersion::Exchange2010Sp2 => Some(ServerVersion::Exchange2010Sp1),
            ServerVersion::Exchange2010Sp1 => Some(ServerVersion::Exchange2010),
            ServerVersion::Exchange2010 => Some(ServerVersion::Exchange2007),
            ServerVersion::Exchange2007 => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ItemClass<'a>(pub &'a str);

impl<'a> ItemClass<'a> {
    fn prefixes(self) -> impl Iterator<Item = &'a str> {
        iter::successors(Some(self.0.trim()), |class| {
            class.rsplit_once('.').map(|(head, _)| head)
        })
    }

    pub fn is_non_mail(self) -> bool {
        self.prefixes().any(|prefix| {
            hashify::set_ignore_case!(
                prefix.as_bytes(),
                "ipm.contact",
                "ipm.distlist",
                "ipm.appointment",
                "ipm.task",
                "ipm.stickynote",
                "ipm.activity",
            )
        })
    }

    fn container(self) -> Option<FolderClass> {
        self.prefixes().find_map(|prefix| {
            hashify::map_ignore_case!(
                prefix.as_bytes(),
                FolderClass,
                "ipf.appointment" => FolderClass::Calendar,
                "ipf.contact" => FolderClass::Contacts,
                "ipf.task" => FolderClass::Skipped,
                "ipf.journal" => FolderClass::Skipped,
                "ipf.stickynote" => FolderClass::Skipped,
                "ipf.configuration" => FolderClass::Skipped,
                "ipf.storeitem" => FolderClass::Skipped,
                "ipf.skypeteams" => FolderClass::Skipped,
                "ipf.files" => FolderClass::Skipped,
                "ipf.note.outlookhomepage" => FolderClass::Skipped,
            )
            .copied()
        })
    }

    fn item(self) -> Option<FolderClass> {
        self.prefixes().find_map(|prefix| {
            hashify::map_ignore_case!(
                prefix.as_bytes(),
                FolderClass,
                "ipm.contact" => FolderClass::Contacts,
                "ipm.distlist" => FolderClass::Contacts,
                "ipm.appointment" => FolderClass::Calendar,
            )
            .copied()
        })
    }

    fn is_note(self) -> bool {
        self.prefixes()
            .any(|prefix| prefix.eq_ignore_ascii_case("ipf.note"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderClass {
    Mail,
    Calendar,
    Contacts,
    Skipped,
}

impl FolderClass {
    pub fn from_ipf(class: &str) -> FolderClass {
        ItemClass(class).container().unwrap_or(FolderClass::Mail)
    }

    pub fn from_item_classes<'a>(classes: impl IntoIterator<Item = &'a str>) -> FolderClass {
        classes
            .into_iter()
            .find_map(|class| ItemClass(class).item())
            .unwrap_or(FolderClass::Mail)
    }

    pub fn is_mail_fallback(class: &str) -> bool {
        let class = ItemClass(class);
        class.container().is_none() && !class.is_note()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarItemType {
    Single,
    RecurringMaster,
    Occurrence,
    Exception,
}

impl CalendarItemType {
    pub fn parse(value: &str) -> Option<CalendarItemType> {
        hashify::map!(
            value.as_bytes(),
            CalendarItemType,
            "Single" => CalendarItemType::Single,
            "RecurringMaster" => CalendarItemType::RecurringMaster,
            "Occurrence" => CalendarItemType::Occurrence,
            "Exception" => CalendarItemType::Exception,
        )
        .copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseCode {
    NoError,
    ItemNotFound,
    AccessDenied,
    InvalidIdMalformed,
    ServerBusy { back_off_ms: Option<u64> },
    TimeoutExpired,
    InvalidSyncStateData,
    MimeContentConversionFailed,
    AttachmentNotFound,
    AuthenticationRequired,
    ImpersonationFailed,
    BatchProcessingStopped,
    InternalServerTransientError,
    MailboxStoreUnavailable,
    ConnectionFailed,
    AdUnavailable,
    InvalidServerVersion,
    IncorrectSchemaVersion,
    InvalidRequest,
    InvalidSchemaVersionForMailboxVersion,
    Other(String),
}

impl ResponseCode {
    pub fn restarts_full_sync(&self) -> bool {
        matches!(
            self,
            ResponseCode::InvalidSyncStateData
                | ResponseCode::InvalidIdMalformed
                | ResponseCode::AccessDenied
                | ResponseCode::Other(_)
        )
    }

    pub fn parse(code: &str) -> ResponseCode {
        hashify::map!(
            code.as_bytes(),
            ResponseCode,
            "NoError" => ResponseCode::NoError,
            "ErrorItemNotFound" => ResponseCode::ItemNotFound,
            "ErrorAccessDenied" => ResponseCode::AccessDenied,
            "ErrorInvalidIdMalformed" => ResponseCode::InvalidIdMalformed,
            "ErrorServerBusy" => ResponseCode::ServerBusy { back_off_ms: None },
            "ErrorTimeoutExpired" => ResponseCode::TimeoutExpired,
            "ErrorInvalidSyncStateData" => ResponseCode::InvalidSyncStateData,
            "ErrorMimeContentConversionFailed" => ResponseCode::MimeContentConversionFailed,
            "ErrorAttachmentNotFound" => ResponseCode::AttachmentNotFound,
            "ErrorAuthenticationRequired" => ResponseCode::AuthenticationRequired,
            "ErrorImpersonationFailed" => ResponseCode::ImpersonationFailed,
            "ErrorBatchProcessingStopped" => ResponseCode::BatchProcessingStopped,
            "ErrorInternalServerTransientError" => ResponseCode::InternalServerTransientError,
            "ErrorMailboxStoreUnavailable" => ResponseCode::MailboxStoreUnavailable,
            "ErrorConnectionFailed" => ResponseCode::ConnectionFailed,
            "ErrorADUnavailable" => ResponseCode::AdUnavailable,
            "ErrorInvalidServerVersion" => ResponseCode::InvalidServerVersion,
            "ErrorIncorrectSchemaVersion" => ResponseCode::IncorrectSchemaVersion,
            "ErrorInvalidRequest" => ResponseCode::InvalidRequest,
            "ErrorInvalidSchemaVersionForMailboxVersion" => ResponseCode::InvalidSchemaVersionForMailboxVersion,
        )
        .cloned()
        .unwrap_or_else(|| ResponseCode::Other(code.to_owned()))
    }

    pub fn as_wire(&self) -> &str {
        match self {
            ResponseCode::NoError => "NoError",
            ResponseCode::ItemNotFound => "ErrorItemNotFound",
            ResponseCode::AccessDenied => "ErrorAccessDenied",
            ResponseCode::InvalidIdMalformed => "ErrorInvalidIdMalformed",
            ResponseCode::ServerBusy { .. } => "ErrorServerBusy",
            ResponseCode::TimeoutExpired => "ErrorTimeoutExpired",
            ResponseCode::InvalidSyncStateData => "ErrorInvalidSyncStateData",
            ResponseCode::MimeContentConversionFailed => "ErrorMimeContentConversionFailed",
            ResponseCode::AttachmentNotFound => "ErrorAttachmentNotFound",
            ResponseCode::AuthenticationRequired => "ErrorAuthenticationRequired",
            ResponseCode::ImpersonationFailed => "ErrorImpersonationFailed",
            ResponseCode::BatchProcessingStopped => "ErrorBatchProcessingStopped",
            ResponseCode::InternalServerTransientError => "ErrorInternalServerTransientError",
            ResponseCode::MailboxStoreUnavailable => "ErrorMailboxStoreUnavailable",
            ResponseCode::ConnectionFailed => "ErrorConnectionFailed",
            ResponseCode::AdUnavailable => "ErrorADUnavailable",
            ResponseCode::InvalidServerVersion => "ErrorInvalidServerVersion",
            ResponseCode::IncorrectSchemaVersion => "ErrorIncorrectSchemaVersion",
            ResponseCode::InvalidRequest => "ErrorInvalidRequest",
            ResponseCode::InvalidSchemaVersionForMailboxVersion => {
                "ErrorInvalidSchemaVersionForMailboxVersion"
            }
            ResponseCode::Other(s) => s.as_str(),
        }
    }
}

impl fmt::Display for ResponseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_mail_item_classes() {
        for class in [
            "IPM.Contact",
            "IPM.DistList",
            "IPM.Appointment",
            "IPM.Appointment.Custom",
            "IPM.Task",
            "ipm.stickynote",
            "IPM.Activity",
        ] {
            assert!(ItemClass(class).is_non_mail(), "{class}");
        }
        for class in [
            "IPM.Note",
            "IPM.Note.SMIME",
            "IPM.Schedule.Meeting.Request",
            "IPM.Schedule.Meeting.Canceled",
            "IPM.Post",
            "REPORT.IPM.Note.NDR",
            "IPM.Contactless",
            "",
        ] {
            assert!(!ItemClass(class).is_non_mail(), "{class}");
        }
    }

    #[test]
    fn folder_kind_from_item_classes() {
        assert_eq!(
            FolderClass::from_item_classes(["IPM.Note", "IPM.Contact"]),
            FolderClass::Contacts
        );
        assert_eq!(
            FolderClass::from_item_classes(["IPM.DistList"]),
            FolderClass::Contacts
        );
        assert_eq!(
            FolderClass::from_item_classes(["IPM.Appointment"]),
            FolderClass::Calendar
        );
        assert_eq!(
            FolderClass::from_item_classes(["IPM.Note", "IPM.Schedule.Meeting.Request"]),
            FolderClass::Mail
        );
        assert_eq!(FolderClass::from_item_classes([]), FolderClass::Mail);
    }

    #[test]
    fn folder_class_mapping() {
        assert_eq!(FolderClass::from_ipf("IPF.Note"), FolderClass::Mail);
        assert_eq!(
            FolderClass::from_ipf("IPF.Appointment"),
            FolderClass::Calendar
        );
        assert_eq!(
            FolderClass::from_ipf("IPF.Appointment.Birthday"),
            FolderClass::Calendar
        );
        assert_eq!(FolderClass::from_ipf("IPF.Contact"), FolderClass::Contacts);
        assert_eq!(FolderClass::from_ipf("IPF.Task"), FolderClass::Skipped);
    }

    #[test]
    fn absent_or_unknown_folder_class_falls_back_to_mail() {
        assert_eq!(FolderClass::from_ipf(""), FolderClass::Mail);
        assert_eq!(FolderClass::from_ipf("   "), FolderClass::Mail);
        assert_eq!(FolderClass::from_ipf("IPF.SomethingNew"), FolderClass::Mail);
        assert!(FolderClass::is_mail_fallback(""));
        assert!(FolderClass::is_mail_fallback("IPF.SomethingNew"));
        assert!(!FolderClass::is_mail_fallback("IPF.Note"));
        assert!(!FolderClass::is_mail_fallback("IPF.Note.OutlookHomepage"));
        assert!(!FolderClass::is_mail_fallback("IPF.Task"));
    }

    #[test]
    fn internal_and_out_of_scope_classes_are_skipped() {
        for c in [
            "IPF.Task",
            "IPF.Journal",
            "IPF.StickyNote",
            "IPF.Configuration",
            "IPF.StoreItem.PdpProfileV2Secured",
            "IPF.SkypeTeams.Message",
            "IPF.Files",
            "IPF.Note.OutlookHomepage",
        ] {
            assert_eq!(FolderClass::from_ipf(c), FolderClass::Skipped, "{c}");
        }
    }

    #[test]
    fn server_version_from_build() {
        assert_eq!(
            ServerVersion::from_build(15, 0),
            ServerVersion::Exchange2013
        );
        assert_eq!(
            ServerVersion::from_build(15, 1),
            ServerVersion::Exchange2016
        );
        assert_eq!(
            ServerVersion::from_build(15, 2),
            ServerVersion::Exchange2016
        );
        assert_eq!(
            ServerVersion::from_build(15, 20),
            ServerVersion::Exchange2016
        );
        assert_eq!(
            ServerVersion::from_build(99, 99),
            ServerVersion::Exchange2016
        );
    }

    #[test]
    fn server_version_from_build_exchange_2010_family() {
        assert_eq!(
            ServerVersion::from_build(14, 0),
            ServerVersion::Exchange2010
        );
        assert_eq!(
            ServerVersion::from_build(14, 1),
            ServerVersion::Exchange2010Sp1
        );
        assert_eq!(
            ServerVersion::from_build(14, 2),
            ServerVersion::Exchange2010Sp2,
            "AWS WorkMail reports MajorVersion=14 MinorVersion=2 (Exchange2010_SP2)"
        );
        assert_eq!(
            ServerVersion::from_build(14, 3),
            ServerVersion::Exchange2010Sp2
        );
        assert_eq!(ServerVersion::from_build(8, 3), ServerVersion::Exchange2007);
    }

    #[test]
    fn server_version_ladder_descends_to_floor() {
        let mut v = ServerVersion::Exchange2016;
        let mut chain = vec![v];
        while let Some(next) = v.lower() {
            chain.push(next);
            v = next;
        }
        assert_eq!(
            chain,
            vec![
                ServerVersion::Exchange2016,
                ServerVersion::Exchange2013Sp1,
                ServerVersion::Exchange2013,
                ServerVersion::Exchange2010Sp2,
                ServerVersion::Exchange2010Sp1,
                ServerVersion::Exchange2010,
                ServerVersion::Exchange2007,
            ]
        );
        assert_eq!(ServerVersion::Exchange2007.lower(), None);
    }

    #[test]
    fn response_code_round_trip() {
        let codes = [
            "NoError",
            "ErrorItemNotFound",
            "ErrorAccessDenied",
            "ErrorInvalidIdMalformed",
            "ErrorServerBusy",
            "ErrorTimeoutExpired",
            "ErrorInvalidSyncStateData",
            "ErrorMimeContentConversionFailed",
            "ErrorAttachmentNotFound",
            "ErrorAuthenticationRequired",
            "ErrorImpersonationFailed",
            "ErrorWhatever",
        ];
        for c in codes {
            assert_eq!(ResponseCode::parse(c).as_wire(), c);
        }
    }
}
