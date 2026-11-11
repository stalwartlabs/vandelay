/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::exchange_ews::soap::write_escaped;
use crate::exchange_ews::types::{DistinguishedFolderId, FolderId, ItemId, ServerVersion};

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

pub fn find_folder_body(parent: FolderRef<'_>, traversal: Traversal) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("<m:FindFolder Traversal=\"");
    out.push_str(traversal.as_str());
    out.push_str("\"><m:FolderShape>");
    out.push_str("<t:BaseShape>Default</t:BaseShape>");
    out.push_str("<t:AdditionalProperties>");
    out.push_str("<t:FieldURI FieldURI=\"folder:FolderClass\"/>");
    out.push_str("<t:FieldURI FieldURI=\"folder:DisplayName\"/>");
    out.push_str("<t:FieldURI FieldURI=\"folder:ParentFolderId\"/>");
    out.push_str("<t:FieldURI FieldURI=\"folder:TotalCount\"/>");
    out.push_str("<t:FieldURI FieldURI=\"folder:ChildFolderCount\"/>");
    out.push_str("<t:ExtendedFieldURI PropertyTag=\"0x10F4\" PropertyType=\"Boolean\"/>");
    out.push_str("</t:AdditionalProperties>");
    out.push_str("</m:FolderShape><m:ParentFolderIds>");
    write_folder_ref(&mut out, parent);
    out.push_str("</m:ParentFolderIds></m:FindFolder>");
    out
}

pub fn get_folder_body(folders: &[FolderRef<'_>], shape: FolderShape) -> String {
    let mut out = String::with_capacity(256 + folders.len() * 64);
    out.push_str("<m:GetFolder><m:FolderShape>");
    out.push_str("<t:BaseShape>");
    out.push_str(shape.as_str());
    out.push_str("</t:BaseShape>");
    out.push_str("</m:FolderShape><m:FolderIds>");
    for f in folders {
        write_folder_ref(&mut out, *f);
    }
    out.push_str("</m:FolderIds></m:GetFolder>");
    out
}

pub fn find_item_body(
    parent: FolderRef<'_>,
    traversal: Traversal,
    offset: u32,
    page_size: u32,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("<m:FindItem Traversal=\"");
    out.push_str(traversal.as_str());
    out.push_str("\"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape></m:ItemShape>");
    out.push_str("<m:IndexedPageItemView MaxEntriesReturned=\"");
    out.push_str(&page_size.to_string());
    out.push_str("\" Offset=\"");
    out.push_str(&offset.to_string());
    out.push_str("\" BasePoint=\"Beginning\"/>");
    out.push_str(
        "<m:SortOrder><t:FieldOrder Order=\"Ascending\">\
         <t:FieldURI FieldURI=\"item:DateTimeCreated\"/></t:FieldOrder></m:SortOrder>",
    );
    out.push_str("<m:ParentFolderIds>");
    write_folder_ref(&mut out, parent);
    out.push_str("</m:ParentFolderIds></m:FindItem>");
    out
}

pub fn sync_folder_items_body(
    folder: &FolderId,
    sync_state: &str,
    max_changes: u32,
    version: ServerVersion,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("<m:SyncFolderItems><m:ItemShape>");
    out.push_str("<t:BaseShape>IdOnly</t:BaseShape>");
    out.push_str("</m:ItemShape><m:SyncFolderId>");
    write_folder_id_element(&mut out, folder);
    out.push_str("</m:SyncFolderId>");
    out.push_str("<m:SyncState>");
    write_escaped(&mut out, sync_state);
    out.push_str("</m:SyncState>");
    out.push_str("<m:MaxChangesReturned>");
    out.push_str(&max_changes.to_string());
    out.push_str("</m:MaxChangesReturned>");
    if version >= ServerVersion::Exchange2010 {
        out.push_str("<m:SyncScope>NormalItems</m:SyncScope>");
    }
    out.push_str("</m:SyncFolderItems>");
    out
}

#[derive(Debug, Clone, Copy)]
pub enum ItemShape {
    Message,
    CalendarItem,
    Contact,
}

pub fn get_item_body(shape: ItemShape, ids: &[ItemId], version: ServerVersion) -> String {
    let mut out = String::with_capacity(256 + ids.len() * 128);
    out.push_str("<m:GetItem><m:ItemShape>");
    match shape {
        ItemShape::Message => {
            out.push_str("<t:BaseShape>IdOnly</t:BaseShape>");
            out.push_str("<t:IncludeMimeContent>true</t:IncludeMimeContent>");
            out.push_str("<t:BodyType>Best</t:BodyType>");
            out.push_str("<t:AdditionalProperties>");
            out.push_str("<t:FieldURI FieldURI=\"item:DateTimeReceived\"/>");
            out.push_str("<t:FieldURI FieldURI=\"message:IsRead\"/>");
            out.push_str("<t:FieldURI FieldURI=\"item:IsDraft\"/>");
            out.push_str("<t:FieldURI FieldURI=\"item:Categories\"/>");
            out.push_str("<t:FieldURI FieldURI=\"item:ParentFolderId\"/>");
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
            out.push_str("<t:BaseShape>IdOnly</t:BaseShape>");
            out.push_str("<t:BodyType>Best</t:BodyType>");
            out.push_str("<t:AdditionalProperties>");
            for f in CALENDAR_FIELDS {
                out.push_str("<t:FieldURI FieldURI=\"");
                out.push_str(f);
                out.push_str("\"/>");
            }
            if version >= ServerVersion::Exchange2010 {
                out.push_str("<t:FieldURI FieldURI=\"calendar:StartTimeZone\"/>");
                out.push_str("<t:FieldURI FieldURI=\"calendar:EndTimeZone\"/>");
            }
            out.push_str("</t:AdditionalProperties>");
        }
        ItemShape::Contact => {
            out.push_str("<t:BaseShape>AllProperties</t:BaseShape>");
        }
    }
    out.push_str("</m:ItemShape><m:ItemIds>");
    for id in ids {
        write_item_id_element(&mut out, id);
    }
    out.push_str("</m:ItemIds></m:GetItem>");
    out
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

pub fn get_attachment_body(ids: &[&str]) -> String {
    let mut out = String::with_capacity(256 + ids.len() * 64);
    out.push_str("<m:GetAttachment><m:AttachmentShape>");
    out.push_str("<t:IncludeMimeContent>true</t:IncludeMimeContent>");
    out.push_str("<t:BodyType>Best</t:BodyType>");
    out.push_str("</m:AttachmentShape><m:AttachmentIds>");
    for id in ids {
        out.push_str("<t:AttachmentId Id=\"");
        write_escaped(&mut out, id);
        out.push_str("\"/>");
    }
    out.push_str("</m:AttachmentIds></m:GetAttachment>");
    out
}

pub fn write_folder_id_element(out: &mut String, folder: &FolderId) {
    out.push_str("<t:FolderId Id=\"");
    write_escaped(out, &folder.id);
    out.push('"');
    if !folder.change_key.is_empty() {
        out.push_str(" ChangeKey=\"");
        write_escaped(out, &folder.change_key);
        out.push('"');
    }
    out.push_str("/>");
}

pub fn write_item_id_element(out: &mut String, id: &ItemId) {
    out.push_str("<t:ItemId Id=\"");
    write_escaped(out, &id.id);
    out.push('"');
    if !id.change_key.is_empty() {
        out.push_str(" ChangeKey=\"");
        write_escaped(out, &id.change_key);
        out.push('"');
    }
    out.push_str("/>");
}

fn write_folder_ref(out: &mut String, parent: FolderRef<'_>) {
    match parent {
        FolderRef::Distinguished(d) => {
            out.push_str("<t:DistinguishedFolderId Id=\"");
            out.push_str(d.as_str());
            out.push_str("\"/>");
        }
        FolderRef::DistinguishedIn(d, mailbox) => {
            out.push_str("<t:DistinguishedFolderId Id=\"");
            out.push_str(d.as_str());
            out.push_str("\"><t:Mailbox><t:EmailAddress>");
            write_escaped(out, mailbox);
            out.push_str("</t:EmailAddress></t:Mailbox></t:DistinguishedFolderId>");
        }
        FolderRef::Concrete(f) => write_folder_id_element(out, f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_folder_uses_deep_traversal_for_msgfolderroot() {
        let body = find_folder_body(
            FolderRef::Distinguished(DistinguishedFolderId::MsgFolderRoot),
            Traversal::Deep,
        );
        assert!(body.contains("Traversal=\"Deep\""));
        assert!(body.contains("<t:DistinguishedFolderId Id=\"msgfolderroot\"/>"));
        assert!(body.contains("folder:FolderClass"));
        assert!(body.contains("folder:DisplayName"));
        assert!(body.contains("folder:ParentFolderId"));
        assert!(body.contains("PropertyTag=\"0x10F4\" PropertyType=\"Boolean\""));
    }

    #[test]
    fn find_folder_archive_uses_archiveroot() {
        let body = find_folder_body(
            FolderRef::Distinguished(DistinguishedFolderId::ArchiveRoot),
            Traversal::Deep,
        );
        assert!(body.contains("<t:DistinguishedFolderId Id=\"archiveroot\"/>"));
    }

    #[test]
    fn distinguished_folder_in_named_mailbox_carries_escaped_address() {
        let body = find_folder_body(
            FolderRef::distinguished(
                DistinguishedFolderId::MsgFolderRoot,
                Some("b&o@corp.example"),
            ),
            Traversal::Deep,
        );
        assert!(body.contains(
            "<t:DistinguishedFolderId Id=\"msgfolderroot\"><t:Mailbox>\
             <t:EmailAddress>b&amp;o@corp.example</t:EmailAddress></t:Mailbox>\
             </t:DistinguishedFolderId>"
        ));
        let body = find_folder_body(
            FolderRef::distinguished(DistinguishedFolderId::MsgFolderRoot, None),
            Traversal::Deep,
        );
        assert!(body.contains("<t:DistinguishedFolderId Id=\"msgfolderroot\"/>"));
    }

    #[test]
    fn find_item_paginates_with_offset_and_page_size() {
        let folder = FolderId::new("FID", "FCK");
        let body = find_item_body(FolderRef::Concrete(&folder), Traversal::Shallow, 200, 50);
        assert!(body.contains("Offset=\"200\""));
        assert!(body.contains("MaxEntriesReturned=\"50\""));
        assert!(body.contains("<t:FolderId Id=\"FID\" ChangeKey=\"FCK\"/>"));
    }

    #[test]
    fn find_item_sorts_by_creation_time_for_stable_paging() {
        let folder = FolderId::new("FID", "FCK");
        let body = find_item_body(FolderRef::Concrete(&folder), Traversal::Shallow, 0, 50);
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
        let body = get_item_body(ItemShape::Message, &ids, ServerVersion::Exchange2013Sp1);
        assert!(body.contains("<t:IncludeMimeContent>true</t:IncludeMimeContent>"));
        assert!(body.contains("<t:ItemId Id=\"I1\" ChangeKey=\"CK1\"/>"));
        assert!(body.contains("<t:ItemId Id=\"I2\"/>"));
    }

    #[test]
    fn item_flag_requested_only_on_exchange_2013_and_later() {
        let ids = [ItemId::new("I1", "")];
        let modern = get_item_body(ItemShape::Message, &ids, ServerVersion::Exchange2013Sp1);
        assert!(modern.contains("item:Flag"));
        let legacy = get_item_body(ItemShape::Message, &ids, ServerVersion::Exchange2010Sp2);
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
        let body = get_item_body(
            ItemShape::CalendarItem,
            &[ItemId::new("X", "")],
            ServerVersion::Exchange2010Sp2,
        );
        assert!(body.contains("calendar:Recurrence"));
        assert!(body.contains("calendar:ModifiedOccurrences"));
        assert!(body.contains("calendar:DeletedOccurrences"));
        assert!(body.contains("calendar:UID"));
        assert!(!body.contains("<t:IncludeMimeContent>"));
    }

    #[test]
    fn get_item_contact_shape_requests_all_properties() {
        let body = get_item_body(
            ItemShape::Contact,
            &[ItemId::new("X", "")],
            ServerVersion::Exchange2013Sp1,
        );
        assert!(body.contains("<t:BaseShape>AllProperties</t:BaseShape>"));
    }

    #[test]
    fn sync_folder_items_body_carries_state_and_max() {
        let folder = FolderId::new("FID", "FCK");
        let body = sync_folder_items_body(&folder, "STATE", 512, ServerVersion::Exchange2013Sp1);
        assert!(body.contains("<m:SyncFolderId><t:FolderId Id=\"FID\" ChangeKey=\"FCK\"/>"));
        assert!(body.contains("<m:SyncState>STATE</m:SyncState>"));
        assert!(body.contains("<m:MaxChangesReturned>512</m:MaxChangesReturned>"));
        assert!(body.contains("<m:SyncScope>NormalItems</m:SyncScope>"));
    }

    #[test]
    fn sync_scope_omitted_below_exchange_2010() {
        let folder = FolderId::new("FID", "");
        let modern = sync_folder_items_body(&folder, "", 100, ServerVersion::Exchange2010);
        assert!(modern.contains("<m:SyncScope>NormalItems</m:SyncScope>"));
        let legacy = sync_folder_items_body(&folder, "", 100, ServerVersion::Exchange2007);
        assert!(
            !legacy.contains("SyncScope"),
            "SyncScope is an Exchange 2010 addition"
        );
    }

    #[test]
    fn calendar_shape_requests_attachments_own_response_and_sensitivity() {
        let body = get_item_body(
            ItemShape::CalendarItem,
            &[ItemId::new("X", "")],
            ServerVersion::Exchange2007,
        );
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
        let modern = get_item_body(
            ItemShape::CalendarItem,
            &[ItemId::new("X", "")],
            ServerVersion::Exchange2010,
        );
        assert!(modern.contains("calendar:StartTimeZone"));
        assert!(modern.contains("calendar:EndTimeZone"));
        let legacy = get_item_body(
            ItemShape::CalendarItem,
            &[ItemId::new("X", "")],
            ServerVersion::Exchange2007,
        );
        assert!(
            !legacy.contains("TimeZone"),
            "StartTimeZone/EndTimeZone are Exchange 2010 additions"
        );
    }

    #[test]
    fn sync_folder_items_empty_state_round_trips() {
        let folder = FolderId::new("FID", "");
        let body = sync_folder_items_body(&folder, "", 100, ServerVersion::Exchange2013Sp1);
        assert!(body.contains("<m:SyncState></m:SyncState>"));
    }

    #[test]
    fn get_attachment_lists_ids() {
        let body = get_attachment_body(&["AID1", "AID2"]);
        assert!(body.contains("<t:AttachmentId Id=\"AID1\"/>"));
        assert!(body.contains("<t:AttachmentId Id=\"AID2\"/>"));
        assert!(body.contains("<t:IncludeMimeContent>true</t:IncludeMimeContent>"));
    }

    #[test]
    fn get_folder_emits_distinguished_ids() {
        let body = get_folder_body(
            &[
                FolderRef::Distinguished(DistinguishedFolderId::Inbox),
                FolderRef::Distinguished(DistinguishedFolderId::SentItems),
            ],
            FolderShape::IdOnly,
        );
        assert!(body.contains("<t:DistinguishedFolderId Id=\"inbox\"/>"));
        assert!(body.contains("<t:DistinguishedFolderId Id=\"sentitems\"/>"));
        assert!(body.contains("<t:BaseShape>IdOnly</t:BaseShape>"));
    }
}
