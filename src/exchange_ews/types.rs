/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxKind {
    Primary,
    Archive,
    PublicFolders,
}

impl MailboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MailboxKind::Primary => "primary",
            MailboxKind::Archive => "archive",
            MailboxKind::PublicFolders => "public-folders",
        }
    }

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

const SKIPPED_CONTAINER_CLASSES: [&str; 8] = [
    "ipf.task",
    "ipf.journal",
    "ipf.stickynote",
    "ipf.configuration",
    "ipf.storeitem",
    "ipf.skypeteams",
    "ipf.files",
    "ipf.note.outlookhomepage",
];

fn class_matches(lower_class: &str, lower_prefix: &str) -> bool {
    lower_class == lower_prefix
        || (lower_class.starts_with(lower_prefix)
            && lower_class[lower_prefix.len()..].starts_with('.'))
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
        let lower = class.trim().to_ascii_lowercase();
        if class_matches(&lower, "ipf.appointment") {
            FolderClass::Calendar
        } else if class_matches(&lower, "ipf.contact") {
            FolderClass::Contacts
        } else if SKIPPED_CONTAINER_CLASSES
            .iter()
            .any(|p| class_matches(&lower, p))
        {
            FolderClass::Skipped
        } else {
            FolderClass::Mail
        }
    }

    pub fn is_mail_fallback(class: &str) -> bool {
        let lower = class.trim().to_ascii_lowercase();
        FolderClass::from_ipf(class) == FolderClass::Mail && !class_matches(&lower, "ipf.note")
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
        match value {
            "Single" => Some(CalendarItemType::Single),
            "RecurringMaster" => Some(CalendarItemType::RecurringMaster),
            "Occurrence" => Some(CalendarItemType::Occurrence),
            "Exception" => Some(CalendarItemType::Exception),
            _ => None,
        }
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
    pub fn parse(code: &str) -> ResponseCode {
        match code {
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
            "ErrorInvalidSchemaVersionForMailboxVersion" => {
                ResponseCode::InvalidSchemaVersionForMailboxVersion
            }
            other => ResponseCode::Other(other.to_owned()),
        }
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
