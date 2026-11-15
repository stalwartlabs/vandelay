/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write;

use crate::exchange::ews::types::{DistinguishedFolderId, FolderId, ItemId, ServerVersion};
use crate::net::xml::XmlEscape;

#[derive(Debug, Clone, Copy)]
pub enum FolderRef<'a> {
    Distinguished(DistinguishedFolderId),
    DistinguishedIn(DistinguishedFolderId, &'a str),
    Concrete(&'a FolderId),
}

impl<'a> FolderRef<'a> {
    pub fn distinguished(id: DistinguishedFolderId, mailbox: Option<&'a str>) -> FolderRef<'a> {
        match mailbox {
            Some(mailbox) => FolderRef::DistinguishedIn(id, mailbox),
            None => FolderRef::Distinguished(id),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FolderShape {
    IdOnly,
}

impl FolderShape {
    fn as_str(self) -> &'static str {
        match self {
            FolderShape::IdOnly => "IdOnly",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Traversal {
    Shallow,
    Deep,
}

impl Traversal {
    fn as_str(self) -> &'static str {
        match self {
            Traversal::Shallow => "Shallow",
            Traversal::Deep => "Deep",
        }
    }
}

const ITEM_CLASS_PROPERTY: &str =
    "<t:AdditionalProperties><t:FieldURI FieldURI=\"item:ItemClass\"/></t:AdditionalProperties>";

const FIND_FOLDER_PROPERTIES: &str = "<t:BaseShape>Default</t:BaseShape>\
     <t:AdditionalProperties>\
     <t:FieldURI FieldURI=\"folder:FolderClass\"/>\
     <t:FieldURI FieldURI=\"folder:DisplayName\"/>\
     <t:FieldURI FieldURI=\"folder:ParentFolderId\"/>\
     <t:FieldURI FieldURI=\"folder:TotalCount\"/>\
     <t:FieldURI FieldURI=\"folder:ChildFolderCount\"/>\
     <t:ExtendedFieldURI PropertyTag=\"0x10F4\" PropertyType=\"Boolean\"/>\
     </t:AdditionalProperties>";

const MESSAGE_SHAPE_HEAD: &str = "<t:BaseShape>IdOnly</t:BaseShape>\
     <t:IncludeMimeContent>true</t:IncludeMimeContent>\
     <t:BodyType>Best</t:BodyType>\
     <t:AdditionalProperties>\
     <t:FieldURI FieldURI=\"item:DateTimeReceived\"/>\
     <t:FieldURI FieldURI=\"message:IsRead\"/>\
     <t:FieldURI FieldURI=\"item:IsDraft\"/>\
     <t:FieldURI FieldURI=\"item:Categories\"/>\
     <t:FieldURI FieldURI=\"item:ParentFolderId\"/>";

const SORT_BY_CREATION: &str = "<m:SortOrder><t:FieldOrder Order=\"Ascending\">\
     <t:FieldURI FieldURI=\"item:DateTimeCreated\"/></t:FieldOrder></m:SortOrder>";

const REQUEST_CAPACITY: usize = 512;
const ITEM_ID_CAPACITY: usize = 128;
const SHORT_ID_CAPACITY: usize = 64;

#[derive(Debug, Clone, Copy)]
pub enum ItemShape {
    Message,
    CalendarItem,
    Contact,
}

#[derive(Debug, Clone, Copy)]
pub enum EwsRequest<'a> {
    FindFolder {
        parent: FolderRef<'a>,
        traversal: Traversal,
    },
    GetFolder {
        folders: &'a [FolderRef<'a>],
        shape: FolderShape,
    },
    FindItem {
        parent: FolderRef<'a>,
        traversal: Traversal,
        offset: u32,
        page_size: u32,
    },
    SyncFolderItems {
        folder: &'a FolderId,
        sync_state: &'a str,
        max_changes: u32,
        version: ServerVersion,
    },
    GetItem {
        shape: ItemShape,
        ids: &'a [ItemId],
        version: ServerVersion,
    },
    GetAttachment {
        ids: &'a [&'a str],
    },
}

impl EwsRequest<'_> {
    pub fn operation(&self) -> &'static str {
        match self {
            EwsRequest::FindFolder { .. } => "FindFolder",
            EwsRequest::GetFolder { .. } => "GetFolder",
            EwsRequest::FindItem { .. } => "FindItem",
            EwsRequest::SyncFolderItems { .. } => "SyncFolderItems",
            EwsRequest::GetItem { .. } => "GetItem",
            EwsRequest::GetAttachment { .. } => "GetAttachment",
        }
    }

    pub fn size_hint(&self) -> usize {
        REQUEST_CAPACITY
            + match self {
                EwsRequest::GetItem { ids, .. } => ids.len() * ITEM_ID_CAPACITY,
                EwsRequest::GetFolder { folders, .. } => folders.len() * SHORT_ID_CAPACITY,
                EwsRequest::GetAttachment { ids } => ids.len() * SHORT_ID_CAPACITY,
                _ => 0,
            }
    }

    pub fn body(&self) -> String {
        let mut out = String::with_capacity(self.size_hint());
        self.write_into(&mut out);
        out
    }

    pub fn write_into(&self, out: &mut String) {
        match *self {
            EwsRequest::FindFolder { parent, traversal } => {
                let _ = write!(
                    out,
                    "<m:FindFolder Traversal=\"{}\"><m:FolderShape>{FIND_FOLDER_PROPERTIES}</m:FolderShape><m:ParentFolderIds>",
                    traversal.as_str()
                );
                parent.write_into(out);
                out.push_str("</m:ParentFolderIds></m:FindFolder>");
            }
            EwsRequest::GetFolder { folders, shape } => {
                let _ = write!(
                    out,
                    "<m:GetFolder><m:FolderShape><t:BaseShape>{}</t:BaseShape></m:FolderShape><m:FolderIds>",
                    shape.as_str()
                );
                for folder in folders {
                    folder.write_into(out);
                }
                out.push_str("</m:FolderIds></m:GetFolder>");
            }
            EwsRequest::FindItem {
                parent,
                traversal,
                offset,
                page_size,
            } => {
                let _ = write!(
                    out,
                    "<m:FindItem Traversal=\"{}\"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>{ITEM_CLASS_PROPERTY}</m:ItemShape>\
                     <m:IndexedPageItemView MaxEntriesReturned=\"{page_size}\" Offset=\"{offset}\" BasePoint=\"Beginning\"/>\
                     {SORT_BY_CREATION}<m:ParentFolderIds>",
                    traversal.as_str()
                );
                parent.write_into(out);
                out.push_str("</m:ParentFolderIds></m:FindItem>");
            }
            EwsRequest::SyncFolderItems {
                folder,
                sync_state,
                max_changes,
                version,
            } => {
                out.push_str("<m:SyncFolderItems><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>");
                out.push_str(ITEM_CLASS_PROPERTY);
                out.push_str("</m:ItemShape><m:SyncFolderId>");
                folder.write_into(out);
                let _ = write!(
                    out,
                    "</m:SyncFolderId><m:SyncState>{}</m:SyncState><m:MaxChangesReturned>{max_changes}</m:MaxChangesReturned>",
                    XmlEscape(sync_state)
                );
                if version >= ServerVersion::Exchange2010 {
                    out.push_str("<m:SyncScope>NormalItems</m:SyncScope>");
                }
                out.push_str("</m:SyncFolderItems>");
            }
            EwsRequest::GetItem {
                shape,
                ids,
                version,
            } => {
                out.push_str("<m:GetItem><m:ItemShape>");
                shape.write_into(out, version);
                out.push_str("</m:ItemShape><m:ItemIds>");
                for id in ids {
                    id.write_into(out);
                }
                out.push_str("</m:ItemIds></m:GetItem>");
            }
            EwsRequest::GetAttachment { ids } => {
                out.push_str(
                    "<m:GetAttachment><m:AttachmentShape>\
                     <t:IncludeMimeContent>true</t:IncludeMimeContent>\
                     <t:BodyType>Best</t:BodyType>\
                     </m:AttachmentShape><m:AttachmentIds>",
                );
                for id in ids {
                    let _ = write!(out, "<t:AttachmentId Id=\"{}\"/>", XmlEscape(id));
                }
                out.push_str("</m:AttachmentIds></m:GetAttachment>");
            }
        }
    }
}

impl ItemShape {
    fn write_into(self, out: &mut String, version: ServerVersion) {
        match self {
            ItemShape::Message => {
                out.push_str(MESSAGE_SHAPE_HEAD);
                if version >= ServerVersion::Exchange2013 {
                    out.push_str("<t:FieldURI FieldURI=\"item:Flag\"/>");
                } else {
                    out.push_str(
                        "<t:ExtendedFieldURI PropertyTag=\"0x1090\" PropertyType=\"Integer\"/>",
                    );
                }
                out.push_str("</t:AdditionalProperties>");
            }
            ItemShape::CalendarItem => {
                out.push_str(
                    "<t:BaseShape>IdOnly</t:BaseShape><t:BodyType>Best</t:BodyType><t:AdditionalProperties>",
                );
                for field in CALENDAR_FIELDS {
                    let _ = write!(out, "<t:FieldURI FieldURI=\"{field}\"/>");
                }
                if version >= ServerVersion::Exchange2010 {
                    out.push_str("<t:FieldURI FieldURI=\"calendar:StartTimeZone\"/>");
                    out.push_str("<t:FieldURI FieldURI=\"calendar:EndTimeZone\"/>");
                }
                out.push_str("</t:AdditionalProperties>");
            }
            ItemShape::Contact => out.push_str("<t:BaseShape>AllProperties</t:BaseShape>"),
        }
    }
}

const CALENDAR_FIELDS: &[&str] = &[
    "calendar:UID",
    "calendar:RecurrenceId",
    "calendar:Start",
    "calendar:End",
    "calendar:OriginalStart",
    "calendar:IsAllDayEvent",
    "calendar:LegacyFreeBusyStatus",
    "calendar:Location",
    "calendar:IsRecurring",
    "calendar:CalendarItemType",
    "calendar:Organizer",
    "calendar:RequiredAttendees",
    "calendar:OptionalAttendees",
    "calendar:Resources",
    "calendar:IsOnlineMeeting",
    "calendar:MeetingWorkspaceUrl",
    "calendar:NetShowUrl",
    "calendar:MyResponseType",
    "item:ReminderIsSet",
    "item:ReminderMinutesBeforeStart",
    "calendar:Recurrence",
    "calendar:ModifiedOccurrences",
    "calendar:DeletedOccurrences",
    "calendar:Duration",
    "item:Subject",
    "item:Body",
    "item:Categories",
    "item:Sensitivity",
    "item:Attachments",
    "item:DateTimeCreated",
    "item:LastModifiedTime",
    "item:ParentFolderId",
];

impl FolderId {
    fn write_into(&self, out: &mut String) {
        let _ = write!(out, "<t:FolderId Id=\"{}\"", XmlEscape(&self.id));
        if !self.change_key.is_empty() {
            let _ = write!(out, " ChangeKey=\"{}\"", XmlEscape(&self.change_key));
        }
        out.push_str("/>");
    }
}

impl ItemId {
    fn write_into(&self, out: &mut String) {
        let _ = write!(out, "<t:ItemId Id=\"{}\"", XmlEscape(&self.id));
        if !self.change_key.is_empty() {
            let _ = write!(out, " ChangeKey=\"{}\"", XmlEscape(&self.change_key));
        }
        out.push_str("/>");
    }
}

impl FolderRef<'_> {
    fn write_into(self, out: &mut String) {
        match self {
            FolderRef::Distinguished(d) => {
                let _ = write!(out, "<t:DistinguishedFolderId Id=\"{}\"/>", d.as_str());
            }
            FolderRef::DistinguishedIn(d, mailbox) => {
                let _ = write!(
                    out,
                    "<t:DistinguishedFolderId Id=\"{}\"><t:Mailbox><t:EmailAddress>{}</t:EmailAddress></t:Mailbox></t:DistinguishedFolderId>",
                    d.as_str(),
                    XmlEscape(mailbox)
                );
            }
            FolderRef::Concrete(f) => f.write_into(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_folder_uses_deep_traversal_for_msgfolderroot() {
        let body = EwsRequest::FindFolder {
            parent: FolderRef::Distinguished(DistinguishedFolderId::MsgFolderRoot),
            traversal: Traversal::Deep,
        }
        .body();
        assert!(body.contains("Traversal=\"Deep\""));
        assert!(body.contains("<t:DistinguishedFolderId Id=\"msgfolderroot\"/>"));
        assert!(body.contains("folder:FolderClass"));
        assert!(body.contains("folder:DisplayName"));
        assert!(body.contains("folder:ParentFolderId"));
        assert!(body.contains("PropertyTag=\"0x10F4\" PropertyType=\"Boolean\""));
    }

    #[test]
    fn find_folder_archive_uses_archiveroot() {
        let body = EwsRequest::FindFolder {
            parent: FolderRef::Distinguished(DistinguishedFolderId::ArchiveRoot),
            traversal: Traversal::Deep,
        }
        .body();
        assert!(body.contains("<t:DistinguishedFolderId Id=\"archiveroot\"/>"));
    }

    #[test]
    fn distinguished_folder_in_named_mailbox_carries_escaped_address() {
        let body = EwsRequest::FindFolder {
            parent: FolderRef::distinguished(
                DistinguishedFolderId::MsgFolderRoot,
                Some("b&o@corp.example"),
            ),
            traversal: Traversal::Deep,
        }
        .body();
        assert!(body.contains(
            "<t:DistinguishedFolderId Id=\"msgfolderroot\"><t:Mailbox>\
             <t:EmailAddress>b&amp;o@corp.example</t:EmailAddress></t:Mailbox>\
             </t:DistinguishedFolderId>"
        ));
        let body = EwsRequest::FindFolder {
            parent: FolderRef::distinguished(DistinguishedFolderId::MsgFolderRoot, None),
            traversal: Traversal::Deep,
        }
        .body();
        assert!(body.contains("<t:DistinguishedFolderId Id=\"msgfolderroot\"/>"));
    }

    #[test]
    fn find_item_paginates_with_offset_and_page_size() {
        let folder = FolderId::new("FID", "FCK");
        let body = EwsRequest::FindItem {
            parent: FolderRef::Concrete(&folder),
            traversal: Traversal::Shallow,
            offset: 200,
            page_size: 50,
        }
        .body();
        assert!(body.contains("Offset=\"200\""));
        assert!(body.contains("MaxEntriesReturned=\"50\""));
        assert!(body.contains("<t:FolderId Id=\"FID\" ChangeKey=\"FCK\"/>"));
    }

    #[test]
    fn find_item_sorts_by_creation_time_for_stable_paging() {
        let folder = FolderId::new("FID", "FCK");
        let body = EwsRequest::FindItem {
            parent: FolderRef::Concrete(&folder),
            traversal: Traversal::Shallow,
            offset: 0,
            page_size: 50,
        }
        .body();
        assert!(body.contains("<m:SortOrder>"));
        assert!(body.contains("Order=\"Ascending\""));
        assert!(body.contains("item:DateTimeCreated"));
        let sort_at = body.find("<m:SortOrder>").unwrap();
        let parents_at = body.find("<m:ParentFolderIds>").unwrap();
        assert!(
            sort_at < parents_at,
            "SortOrder must precede ParentFolderIds"
        );
    }

    #[test]
    fn get_item_message_shape_requests_mime() {
        let ids = vec![ItemId::new("I1", "CK1"), ItemId::new("I2", "")];
        let body = EwsRequest::GetItem {
            shape: ItemShape::Message,
            ids: &ids,
            version: ServerVersion::Exchange2013Sp1,
        }
        .body();
        assert!(body.contains("<t:IncludeMimeContent>true</t:IncludeMimeContent>"));
        assert!(body.contains("<t:ItemId Id=\"I1\" ChangeKey=\"CK1\"/>"));
        assert!(body.contains("<t:ItemId Id=\"I2\"/>"));
    }

    #[test]
    fn item_flag_requested_only_on_exchange_2013_and_later() {
        let ids = [ItemId::new("I1", "")];
        let modern = EwsRequest::GetItem {
            shape: ItemShape::Message,
            ids: &ids,
            version: ServerVersion::Exchange2013Sp1,
        }
        .body();
        assert!(modern.contains("item:Flag"));
        let legacy = EwsRequest::GetItem {
            shape: ItemShape::Message,
            ids: &ids,
            version: ServerVersion::Exchange2010Sp2,
        }
        .body();
        assert!(
            !legacy.contains("item:Flag"),
            "item:Flag is an Exchange 2013 schema addition; must be omitted on 2010"
        );
        assert!(
            legacy.contains("PropertyTag=\"0x1090\""),
            "on pre-2013 the follow-up flag must be fetched via the PidTagFlagStatus extended property"
        );
        assert!(
            !modern.contains("0x1090"),
            "the extended-property fallback is only used when item:Flag is unavailable"
        );
        assert!(legacy.contains("item:DateTimeReceived"));
        assert!(
            modern.contains("\"message:IsRead\"") && !modern.contains("\"item:IsRead\""),
            "IsRead is a MessageType property; its FieldURI is message:IsRead"
        );
    }

    #[test]
    fn get_item_calendar_shape_lists_calendar_fields() {
        let body = EwsRequest::GetItem {
            shape: ItemShape::CalendarItem,
            ids: &[ItemId::new("X", "")],
            version: ServerVersion::Exchange2010Sp2,
        }
        .body();
        assert!(body.contains("calendar:Recurrence"));
        assert!(body.contains("calendar:ModifiedOccurrences"));
        assert!(body.contains("calendar:DeletedOccurrences"));
        assert!(body.contains("calendar:UID"));
        assert!(!body.contains("<t:IncludeMimeContent>"));
    }

    #[test]
    fn get_item_contact_shape_requests_all_properties() {
        let body = EwsRequest::GetItem {
            shape: ItemShape::Contact,
            ids: &[ItemId::new("X", "")],
            version: ServerVersion::Exchange2013Sp1,
        }
        .body();
        assert!(body.contains("<t:BaseShape>AllProperties</t:BaseShape>"));
    }

    #[test]
    fn sync_folder_items_body_carries_state_and_max() {
        let folder = FolderId::new("FID", "FCK");
        let body = EwsRequest::SyncFolderItems {
            folder: &folder,
            sync_state: "STATE",
            max_changes: 512,
            version: ServerVersion::Exchange2013Sp1,
        }
        .body();
        assert!(body.contains("<m:SyncFolderId><t:FolderId Id=\"FID\" ChangeKey=\"FCK\"/>"));
        assert!(body.contains("<m:SyncState>STATE</m:SyncState>"));
        assert!(body.contains("<m:MaxChangesReturned>512</m:MaxChangesReturned>"));
        assert!(body.contains("<m:SyncScope>NormalItems</m:SyncScope>"));
        assert!(body.contains("FieldURI=\"item:ItemClass\""));
    }

    #[test]
    fn find_item_requests_item_class() {
        let folder = FolderId::new("FID", "FCK");
        let body = EwsRequest::FindItem {
            parent: FolderRef::Concrete(&folder),
            traversal: Traversal::Shallow,
            offset: 0,
            page_size: 10,
        }
        .body();
        assert!(body.contains(
            "<t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI=\"item:ItemClass\"/>"
        ));
    }

    #[test]
    fn sync_scope_omitted_below_exchange_2010() {
        let folder = FolderId::new("FID", "");
        let modern = EwsRequest::SyncFolderItems {
            folder: &folder,
            sync_state: "",
            max_changes: 100,
            version: ServerVersion::Exchange2010,
        }
        .body();
        assert!(modern.contains("<m:SyncScope>NormalItems</m:SyncScope>"));
        let legacy = EwsRequest::SyncFolderItems {
            folder: &folder,
            sync_state: "",
            max_changes: 100,
            version: ServerVersion::Exchange2007,
        }
        .body();
        assert!(
            !legacy.contains("SyncScope"),
            "SyncScope is an Exchange 2010 addition"
        );
    }

    #[test]
    fn calendar_shape_requests_attachments_own_response_and_sensitivity() {
        let body = EwsRequest::GetItem {
            shape: ItemShape::CalendarItem,
            ids: &[ItemId::new("X", "")],
            version: ServerVersion::Exchange2007,
        }
        .body();
        for field in [
            "item:Attachments",
            "calendar:MyResponseType",
            "item:Sensitivity",
        ] {
            assert!(body.contains(&format!("\"{field}\"")), "{field}");
        }
    }

    #[test]
    fn calendar_timezone_fields_gated_on_exchange_2010() {
        let modern = EwsRequest::GetItem {
            shape: ItemShape::CalendarItem,
            ids: &[ItemId::new("X", "")],
            version: ServerVersion::Exchange2010,
        }
        .body();
        assert!(modern.contains("calendar:StartTimeZone"));
        assert!(modern.contains("calendar:EndTimeZone"));
        let legacy = EwsRequest::GetItem {
            shape: ItemShape::CalendarItem,
            ids: &[ItemId::new("X", "")],
            version: ServerVersion::Exchange2007,
        }
        .body();
        assert!(
            !legacy.contains("TimeZone"),
            "StartTimeZone/EndTimeZone are Exchange 2010 additions"
        );
    }

    #[test]
    fn sync_folder_items_empty_state_round_trips() {
        let folder = FolderId::new("FID", "");
        let body = EwsRequest::SyncFolderItems {
            folder: &folder,
            sync_state: "",
            max_changes: 100,
            version: ServerVersion::Exchange2013Sp1,
        }
        .body();
        assert!(body.contains("<m:SyncState></m:SyncState>"));
    }

    #[test]
    fn get_attachment_lists_ids() {
        let body = EwsRequest::GetAttachment {
            ids: &["AID1", "AID2"],
        }
        .body();
        assert!(body.contains("<t:AttachmentId Id=\"AID1\"/>"));
        assert!(body.contains("<t:AttachmentId Id=\"AID2\"/>"));
        assert!(body.contains("<t:IncludeMimeContent>true</t:IncludeMimeContent>"));
    }

    #[test]
    fn get_folder_emits_distinguished_ids() {
        let body = EwsRequest::GetFolder {
            folders: &[
                FolderRef::Distinguished(DistinguishedFolderId::Inbox),
                FolderRef::Distinguished(DistinguishedFolderId::SentItems),
            ],
            shape: FolderShape::IdOnly,
        }
        .body();
        assert!(body.contains("<t:DistinguishedFolderId Id=\"inbox\"/>"));
        assert!(body.contains("<t:DistinguishedFolderId Id=\"sentitems\"/>"));
        assert!(body.contains("<t:BaseShape>IdOnly</t:BaseShape>"));
    }
}
