/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::*;
use crate::exchange::ews::types::{CalendarItemType, ResponseCode, ServerVersion};

const NS: &str = " xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                  xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
                  xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\"";

#[test]
fn server_busy_fault_extracts_backoff_milliseconds() {
    let body = format!(
        "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"1\"/></soap:Header><soap:Body>\
         <soap:Fault><faultcode>soap:Server</faultcode><faultstring>busy</faultstring><detail>\
         <ResponseCode xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">ErrorServerBusy</ResponseCode>\
         <t:MessageXml><t:Value Name=\"BackOffMilliseconds\">297749</t:Value></t:MessageXml>\
         </detail></soap:Fault></soap:Body></soap:Envelope>"
    );
    let env = EnvelopeKind::read(body.as_bytes()).unwrap();
    match env {
        EnvelopeKind::Fault { fault, version } => {
            assert!(matches!(
                fault.response_code,
                ResponseCode::ServerBusy {
                    back_off_ms: Some(297749)
                }
            ));
            assert_eq!(fault.back_off_ms, Some(297749));
            assert_eq!(
                version.to_server_version(),
                Some(ServerVersion::Exchange2016)
            );
        }
        _ => panic!("expected fault"),
    }
}

#[test]
fn body_envelope_returns_version_only() {
    let body = format!(
        "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"1\"/></soap:Header>\
         <soap:Body><m:FindFolderResponse/></soap:Body></soap:Envelope>"
    );
    let env = EnvelopeKind::read(body.as_bytes()).unwrap();
    assert!(matches!(env, EnvelopeKind::Body { .. }));
}

#[test]
fn find_folder_parses_two_folders() {
    let body = format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"2\" IncludesLastItemInRange=\"true\">\
         <t:Folders>\
         <t:Folder><t:FolderId Id=\"F1\" ChangeKey=\"C1\"/><t:ParentFolderId Id=\"ROOT\"/>\
           <t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Inbox</t:DisplayName>\
           <t:TotalCount>7</t:TotalCount><t:ChildFolderCount>0</t:ChildFolderCount></t:Folder>\
         <t:CalendarFolder><t:FolderId Id=\"F2\"/><t:ParentFolderId Id=\"ROOT\"/>\
           <t:FolderClass>IPF.Appointment</t:FolderClass><t:DisplayName>Calendar</t:DisplayName>\
           </t:CalendarFolder>\
         </t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    );
    let r = FindFolderResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(r.folders.len(), 2);
    assert_eq!(r.folders[0].folder_id.id, "F1");
    assert_eq!(r.folders[0].folder_id.change_key, "C1");
    assert_eq!(r.folders[0].display_name, "Inbox");
    assert_eq!(r.folders[0].folder_class, "IPF.Note");
    assert_eq!(r.folders[0].parent_id.as_deref(), Some("ROOT"));
    assert_eq!(r.folders[1].element, FolderElement::CalendarFolder);
    assert!(!r.more);
    assert!(!r.folders[0].is_hidden);
}

#[test]
fn find_folder_reads_pr_attr_hidden() {
    let body = format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"2\" IncludesLastItemInRange=\"true\">\
         <t:Folders>\
         <t:ContactsFolder><t:FolderId Id=\"RC\"/><t:ParentFolderId Id=\"CONTACTS\"/>\
           <t:FolderClass>IPF.Contact</t:FolderClass><t:DisplayName>Recipient Cache</t:DisplayName>\
           <t:ExtendedProperty><t:ExtendedFieldURI PropertyTag=\"0x10f4\" PropertyType=\"Boolean\"/>\
           <t:Value>true</t:Value></t:ExtendedProperty></t:ContactsFolder>\
         <t:Folder><t:FolderId Id=\"F1\"/><t:ParentFolderId Id=\"ROOT\"/>\
           <t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Inbox</t:DisplayName>\
           <t:ExtendedProperty><t:ExtendedFieldURI PropertyTag=\"0x10f4\" PropertyType=\"Boolean\"/>\
           <t:Value>false</t:Value></t:ExtendedProperty></t:Folder>\
         </t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    );
    let r = FindFolderResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(r.folders.len(), 2);
    assert!(r.folders[0].is_hidden);
    assert_eq!(r.folders[0].display_name, "Recipient Cache");
    assert!(!r.folders[1].is_hidden);
    assert_eq!(r.folders[1].display_name, "Inbox");
}

#[test]
fn find_item_reads_item_class() {
    let body = format!(
        "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"2\" IncludesLastItemInRange=\"true\"><t:Items>\
         <t:Contact><t:ItemId Id=\"C1\" ChangeKey=\"k\"/><t:ItemClass>IPM.Contact</t:ItemClass></t:Contact>\
         <t:Message><t:ItemId Id=\"M1\" ChangeKey=\"k\"/></t:Message>\
         </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
    );
    let r = FindItemResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(r.items[0].item_class.as_deref(), Some("IPM.Contact"));
    assert_eq!(r.items[1].item_class, None);
}

#[test]
fn sync_folder_items_reads_item_class() {
    let body = format!(
        "<m:SyncFolderItemsResponse{NS}><m:ResponseMessages><m:SyncFolderItemsResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:SyncState>AAAA</m:SyncState>\
         <m:IncludesLastItemInRange>true</m:IncludesLastItemInRange>\
         <m:Changes>\
           <t:Create><t:Task><t:ItemId Id=\"T1\"/><t:ItemClass>IPM.Task</t:ItemClass></t:Task></t:Create>\
           <t:Update><t:Message><t:ItemId Id=\"M1\"/><t:ItemClass>IPM.Note</t:ItemClass></t:Message></t:Update>\
         </m:Changes></m:SyncFolderItemsResponseMessage></m:ResponseMessages></m:SyncFolderItemsResponse>"
    );
    let r = SyncFolderItemsResponse::parse(body.as_bytes()).unwrap();
    let classes: Vec<Option<&str>> = r
        .changes
        .iter()
        .map(|c| match c {
            SyncChange::Create { item_class, .. } | SyncChange::Update { item_class, .. } => {
                item_class.as_deref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(classes, [Some("IPM.Task"), Some("IPM.Note")]);
}

#[test]
fn find_item_opens_an_entry_for_every_item_element() {
    let body = format!(
        "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"4\" IncludesLastItemInRange=\"true\"><t:Items>\
         <t:Message><t:ItemId Id=\"M1\" ChangeKey=\"k\"/></t:Message>\
         <t:Task><t:ItemId Id=\"T1\" ChangeKey=\"k\"/></t:Task>\
         <t:PostItem><t:ItemId Id=\"P1\" ChangeKey=\"k\"/></t:PostItem>\
         <t:FutureItem><t:ItemId Id=\"F1\" ChangeKey=\"k\"/></t:FutureItem>\
         </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
    );
    let r = FindItemResponse::parse(body.as_bytes()).unwrap();
    let ids: Vec<(&str, &str)> = r
        .items
        .iter()
        .map(|i| (i.element.as_str(), i.id.id.as_str()))
        .collect();
    assert_eq!(
        ids,
        [
            ("Message", "M1"),
            ("Task", "T1"),
            ("PostItem", "P1"),
            ("FutureItem", "F1")
        ]
    );
}

#[test]
fn find_item_ignores_item_ids_nested_in_mailboxes() {
    let body = format!(
        "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\"><t:Items>\
         <t:CalendarItem><t:ItemId Id=\"E1\" ChangeKey=\"k\"/>\
         <t:Organizer><t:Mailbox><t:EmailAddress>bob@example.com</t:EmailAddress>\
         <t:ItemId Id=\"CONTACT-BOB\"/></t:Mailbox></t:Organizer></t:CalendarItem>\
         </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
    );
    let r = FindItemResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].id.id, "E1");
}

#[test]
fn calendar_item_keeps_its_own_id_when_attendees_carry_contact_ids() {
    let xml = "<m:Items xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
               xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
               <t:CalendarItem><t:ItemId Id=\"EVENT-A\" ChangeKey=\"CK\"/>\
               <t:Organizer><t:Mailbox><t:EmailAddress>alice@example.com</t:EmailAddress>\
               <t:MailboxType>Contact</t:MailboxType><t:ItemId Id=\"CONTACT-ALICE\"/></t:Mailbox></t:Organizer>\
               <t:RequiredAttendees><t:Attendee><t:Mailbox>\
               <t:EmailAddress>bob@example.com</t:EmailAddress>\
               <t:MailboxType>Contact</t:MailboxType><t:ItemId Id=\"CONTACT-BOB\"/>\
               </t:Mailbox></t:Attendee></t:RequiredAttendees></t:CalendarItem></m:Items>";
    let item = CalendarItemRaw::parse(xml).unwrap();
    assert_eq!(item.id.id, "EVENT-A");
    assert_eq!(item.id.change_key, "CK");
    assert_eq!(item.required_attendees.len(), 1);
}

#[test]
fn message_item_keeps_its_own_id_when_recipients_carry_contact_ids() {
    let xml = "<m:Items xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
               xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
               <t:MeetingRequest><t:ItemId Id=\"MSG-A\" ChangeKey=\"CK\"/>\
               <t:ToRecipients><t:Mailbox><t:EmailAddress>bob@example.com</t:EmailAddress>\
               <t:ItemId Id=\"CONTACT-BOB\"/></t:Mailbox></t:ToRecipients></t:MeetingRequest></m:Items>";
    let item = MessageItem::parse(xml).unwrap();
    assert_eq!(item.id.id, "MSG-A");
    assert_eq!(item.element, "MeetingRequest");
}

#[test]
fn distribution_list_keeps_its_own_id_when_members_carry_contact_ids() {
    let xml = "<m:Items xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
               xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
               <t:DistributionList><t:ItemId Id=\"DL-A\" ChangeKey=\"CK\"/>\
               <t:DisplayName>Team</t:DisplayName><t:Members><t:Member><t:Mailbox>\
               <t:Name>Bob</t:Name><t:EmailAddress>bob@example.com</t:EmailAddress>\
               <t:MailboxType>Contact</t:MailboxType><t:ItemId Id=\"CONTACT-BOB\"/>\
               </t:Mailbox></t:Member></t:Members></t:DistributionList></m:Items>";
    let item = ContactItemRaw::parse(xml).unwrap();
    assert_eq!(item.id.id, "DL-A");
    assert!(item.is_group);
    assert_eq!(item.members.len(), 1);
    assert_eq!(item.members[0].email.as_deref(), Some("bob@example.com"));
}

#[test]
fn find_item_parses_two_messages_with_pagination_flag() {
    let body = format!(
        "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"3\" IncludesLastItemInRange=\"false\">\
         <t:Items>\
           <t:Message><t:ItemId Id=\"I1\" ChangeKey=\"CK1\"/></t:Message>\
           <t:Message><t:ItemId Id=\"I2\" ChangeKey=\"CK2\"/></t:Message>\
         </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
    );
    let r = FindItemResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(r.items.len(), 2);
    assert_eq!(r.items[0].id.id, "I1");
    assert_eq!(r.items[0].id.change_key, "CK1");
    assert_eq!(r.items[1].id.id, "I2");
    assert!(r.more);
    assert_eq!(r.total_in_view, Some(3));
}

#[test]
fn parse_response_messages_carries_inner_xml_and_codes() {
    let body = format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages>\
         <m:GetItemResponseMessage ResponseClass=\"Success\">\
           <m:ResponseCode>NoError</m:ResponseCode>\
           <m:Items><t:Message><t:ItemId Id=\"X1\" ChangeKey=\"K1\"/>\
             <t:Subject>Hi</t:Subject><t:IsRead>true</t:IsRead>\
             <t:MimeContent CharacterSet=\"UTF-8\">SGVsbG8=</t:MimeContent>\
             </t:Message></m:Items></m:GetItemResponseMessage>\
         <m:GetItemResponseMessage ResponseClass=\"Error\">\
           <m:ResponseCode>ErrorItemNotFound</m:ResponseCode>\
           <m:MessageText>not found</m:MessageText>\
           </m:GetItemResponseMessage>\
         </m:ResponseMessages></m:GetItemResponse>"
    );
    let r = ResponseMessage::parse_all(body.as_bytes(), "GetItemResponseMessage").unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].success);
    assert!(r[0].inner_xml.contains("<t:ItemId"));
    assert!(r[0].inner_xml.contains("SGVsbG8="));
    let parsed = MessageItem::parse(&r[0].inner_xml).unwrap();
    assert_eq!(parsed.id.id, "X1");
    assert_eq!(parsed.subject.as_deref(), Some("Hi"));
    assert_eq!(parsed.is_read, Some(true));
    assert_eq!(parsed.mime_content.as_deref(), Some("SGVsbG8="));
    assert!(!r[1].success);
    assert!(matches!(r[1].response_code, ResponseCode::ItemNotFound));
}

#[test]
fn pid_tag_flag_status_extended_property_marks_message_flagged() {
    fn parse_one(message_xml: &str) -> MessageItem {
        let body = format!(
            "<m:GetItemResponse{NS}><m:ResponseMessages>\
             <m:GetItemResponseMessage ResponseClass=\"Success\">\
               <m:ResponseCode>NoError</m:ResponseCode>\
               <m:Items>{message_xml}</m:Items>\
             </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
        );
        let r = ResponseMessage::parse_all(body.as_bytes(), "GetItemResponseMessage").unwrap();
        MessageItem::parse(&r[0].inner_xml).unwrap()
    }

    let flagged = parse_one(
        "<t:Message><t:ItemId Id=\"F1\" ChangeKey=\"K\"/>\
         <t:Subject>Flagged</t:Subject>\
         <t:ExtendedProperty>\
           <t:ExtendedFieldURI PropertyTag=\"0x1090\" PropertyType=\"Integer\"/>\
           <t:Value>2</t:Value>\
         </t:ExtendedProperty>\
         <t:IsRead>true</t:IsRead></t:Message>",
    );
    assert_eq!(flagged.flag_status.as_deref(), Some("Flagged"));
    assert_eq!(flagged.is_read, Some(true));

    let cleared = parse_one(
        "<t:Message><t:ItemId Id=\"F2\" ChangeKey=\"K\"/>\
         <t:ExtendedProperty>\
           <t:ExtendedFieldURI PropertyTag=\"0x1090\" PropertyType=\"Integer\"/>\
           <t:Value>0</t:Value>\
         </t:ExtendedProperty></t:Message>",
    );
    assert_eq!(cleared.flag_status, None);
}

#[test]
fn native_item_flag_still_parses_on_exchange_2013_plus() {
    let body = format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages>\
         <m:GetItemResponseMessage ResponseClass=\"Success\">\
           <m:ResponseCode>NoError</m:ResponseCode>\
           <m:Items><t:Message><t:ItemId Id=\"M\" ChangeKey=\"K\"/>\
             <t:Subject>Native</t:Subject>\
             <t:Flag><t:FlagStatus>Flagged</t:FlagStatus></t:Flag>\
             <t:IsRead>false</t:IsRead></t:Message></m:Items>\
         </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
    );
    let r = ResponseMessage::parse_all(body.as_bytes(), "GetItemResponseMessage").unwrap();
    let p = MessageItem::parse(&r[0].inner_xml).unwrap();
    assert_eq!(
        p.flag_status.as_deref(),
        Some("Flagged"),
        "the 2013+ native item:Flag path must be unaffected by the pre-2013 extended-property fallback"
    );
    assert_eq!(p.is_read, Some(false));
}

#[test]
fn sync_folder_items_emits_create_delete_readflag() {
    let body = format!(
        "<m:SyncFolderItemsResponse{NS}><m:ResponseMessages><m:SyncFolderItemsResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:SyncState>AAAA</m:SyncState>\
         <m:IncludesLastItemInRange>true</m:IncludesLastItemInRange>\
         <m:Changes>\
           <t:Create><t:Message><t:ItemId Id=\"I1\" ChangeKey=\"K1\"/></t:Message></t:Create>\
           <t:Delete><t:ItemId Id=\"I2\"/></t:Delete>\
           <t:ReadFlagChange><t:ItemId Id=\"I3\"/><t:IsRead>true</t:IsRead></t:ReadFlagChange>\
         </m:Changes></m:SyncFolderItemsResponseMessage></m:ResponseMessages></m:SyncFolderItemsResponse>"
    );
    let r = SyncFolderItemsResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(r.sync_state, "AAAA");
    assert!(!r.more);
    assert_eq!(r.changes.len(), 3);
    match &r.changes[0] {
        SyncChange::Create { id, element, .. } => {
            assert_eq!(id.id, "I1");
            assert_eq!(element, "Message");
        }
        _ => panic!("expected Create"),
    }
    match &r.changes[2] {
        SyncChange::ReadFlagChange { id, is_read } => {
            assert_eq!(id.id, "I3");
            assert!(*is_read);
        }
        _ => panic!("expected ReadFlagChange"),
    }
}

#[test]
fn calendar_master_recurrence_and_occurrences_round_trip() {
    let body = format!(
        "<t:CalendarItem{NS}>\
         <t:ItemId Id=\"M1\" ChangeKey=\"K1\"/>\
         <t:Subject>Daily</t:Subject>\
         <t:UID>uid-1</t:UID>\
         <t:Start>2025-06-15T14:00:00Z</t:Start>\
         <t:End>2025-06-15T15:00:00Z</t:End>\
         <t:CalendarItemType>RecurringMaster</t:CalendarItemType>\
         <t:Recurrence>\
           <t:DailyRecurrence><t:Interval>1</t:Interval></t:DailyRecurrence>\
           <t:NumberedRecurrence><t:StartDate>2025-06-15</t:StartDate><t:NumberOfOccurrences>3</t:NumberOfOccurrences></t:NumberedRecurrence>\
         </t:Recurrence>\
         <t:ModifiedOccurrences>\
           <t:Occurrence><t:ItemId Id=\"OCC1\"/><t:Start>2025-06-16T15:00:00Z</t:Start><t:End>2025-06-16T16:30:00Z</t:End><t:OriginalStart>2025-06-16T14:00:00Z</t:OriginalStart></t:Occurrence>\
         </t:ModifiedOccurrences>\
         <t:DeletedOccurrences>\
           <t:DeletedOccurrence><t:Start>2025-06-17T14:00:00Z</t:Start></t:DeletedOccurrence>\
         </t:DeletedOccurrences>\
         </t:CalendarItem>"
    );
    let parsed = CalendarItemRaw::parse(&body).unwrap();
    assert_eq!(parsed.id.id, "M1");
    assert_eq!(parsed.uid.as_deref(), Some("uid-1"));
    assert_eq!(
        parsed.calendar_item_type,
        Some(CalendarItemType::RecurringMaster)
    );
    assert_eq!(parsed.modified_occurrences.len(), 1);
    assert_eq!(
        parsed.modified_occurrences[0].original_start.as_deref(),
        Some("2025-06-16T14:00:00Z")
    );
    assert_eq!(parsed.deleted_occurrences.len(), 1);
    assert_eq!(
        parsed.deleted_occurrences[0].start.as_deref(),
        Some("2025-06-17T14:00:00Z")
    );
    let rec = parsed.recurrence.unwrap();
    assert!(matches!(
        rec.pattern,
        Some(RecurrencePattern::Daily { interval: 1 })
    ));
    assert!(matches!(
        rec.range,
        Some(RecurrenceRange::Numbered {
            number_of_occurrences: 3,
            ..
        })
    ));
}

#[test]
fn end_date_recurrence_wire_shape_with_trailing_z_yields_clean_until() {
    let body = format!(
        "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/>\
         <t:Subject>RecDailyEnd</t:Subject><t:UID>u</t:UID>\
         <t:Start>2026-07-01T15:00:00Z</t:Start><t:End>2026-07-01T15:30:00Z</t:End>\
         <t:CalendarItemType>RecurringMaster</t:CalendarItemType>\
         <t:Recurrence>\
           <t:DailyRecurrence><t:Interval>2</t:Interval></t:DailyRecurrence>\
           <t:EndDateRecurrence><t:StartDate>2026-07-01Z</t:StartDate><t:EndDate>2026-08-01Z</t:EndDate></t:EndDateRecurrence>\
         </t:Recurrence></t:CalendarItem>"
    );
    let parsed = CalendarItemRaw::parse(&body).unwrap();
    let rule = parsed.recurrence.as_ref().unwrap().to_jscalendar().unwrap();
    assert_eq!(rule["frequency"], "daily");
    assert_eq!(rule["interval"], 2);
    assert_eq!(
        rule["until"], "2026-08-01T23:59:59",
        "trailing Z on EndDate must be stripped, not embedded mid-string"
    );
}

#[test]
fn absolute_and_relative_recurrence_wire_shapes_round_trip() {
    let cases = [
        (
            "<t:AbsoluteMonthlyRecurrence><t:Interval>1</t:Interval><t:DayOfMonth>15</t:DayOfMonth></t:AbsoluteMonthlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
            "monthly",
        ),
        (
            "<t:RelativeMonthlyRecurrence><t:Interval>1</t:Interval><t:DaysOfWeek>Friday</t:DaysOfWeek><t:DayOfWeekIndex>Last</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
            "monthly",
        ),
        (
            "<t:AbsoluteYearlyRecurrence><t:DayOfMonth>1</t:DayOfMonth><t:Month>January</t:Month></t:AbsoluteYearlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
            "yearly",
        ),
        (
            "<t:RelativeYearlyRecurrence><t:DaysOfWeek>Monday</t:DaysOfWeek><t:DayOfWeekIndex>First</t:DayOfWeekIndex><t:Month>September</t:Month></t:RelativeYearlyRecurrence><t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence>",
            "yearly",
        ),
    ];
    for (rec_xml, freq) in cases {
        let body = format!(
            "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/><t:UID>u</t:UID>\
             <t:Start>2026-07-01T15:00:00Z</t:Start><t:End>2026-07-01T15:30:00Z</t:End>\
             <t:Recurrence>{rec_xml}</t:Recurrence></t:CalendarItem>"
        );
        let parsed = CalendarItemRaw::parse(&body).unwrap();
        let rule = parsed.recurrence.as_ref().unwrap().to_jscalendar().unwrap();
        assert_eq!(rule["frequency"], freq, "for {rec_xml}");
    }
    let body = format!(
        "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/><t:UID>u</t:UID>\
         <t:Start>2026-07-01T15:00:00Z</t:Start><t:End>2026-07-01T15:30:00Z</t:End>\
         <t:Recurrence><t:RelativeMonthlyRecurrence><t:Interval>1</t:Interval>\
         <t:DaysOfWeek>Friday</t:DaysOfWeek><t:DayOfWeekIndex>Last</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence>\
         <t:NoEndRecurrence><t:StartDate>2026-07-01Z</t:StartDate></t:NoEndRecurrence></t:Recurrence></t:CalendarItem>"
    );
    let parsed = CalendarItemRaw::parse(&body).unwrap();
    let rule = parsed.recurrence.as_ref().unwrap().to_jscalendar().unwrap();
    assert_eq!(rule["byDay"][0]["day"], "fr");
    assert_eq!(rule["byDay"][0]["nthOfPeriod"], -1);
}

#[test]
fn attendee_and_organizer_routing_type_is_parsed() {
    let body = format!(
        "<t:CalendarItem{NS}><t:ItemId Id=\"M\" ChangeKey=\"K\"/><t:UID>u</t:UID>\
         <t:Organizer><t:Mailbox><t:Name>Org</t:Name>\
           <t:EmailAddress>/o=ExLabs/cn=org</t:EmailAddress><t:RoutingType>EX</t:RoutingType></t:Mailbox></t:Organizer>\
         <t:RequiredAttendees><t:Attendee><t:Mailbox><t:Name>Att</t:Name>\
           <t:EmailAddress>/o=ExLabs/cn=att</t:EmailAddress><t:RoutingType>EX</t:RoutingType></t:Mailbox>\
           <t:ResponseType>Accept</t:ResponseType></t:Attendee></t:RequiredAttendees>\
         </t:CalendarItem>"
    );
    let parsed = CalendarItemRaw::parse(&body).unwrap();
    assert_eq!(parsed.organizer_routing_type.as_deref(), Some("EX"));
    assert_eq!(parsed.required_attendees.len(), 1);
    assert_eq!(
        parsed.required_attendees[0].routing_type.as_deref(),
        Some("EX")
    );
    assert_eq!(
        parsed.required_attendees[0].email.as_deref(),
        Some("/o=ExLabs/cn=att")
    );
}

#[test]
fn calendar_parses_own_response_sensitivity_and_attachments() {
    let body = format!(
        "<t:CalendarItem{NS}>\
         <t:ItemId Id=\"C1\" ChangeKey=\"K1\"/>\
         <t:Sensitivity>Private</t:Sensitivity>\
         <t:Attachments><t:FileAttachment><t:AttachmentId Id=\"ATT1\"/>\
           <t:Name>agenda.pdf</t:Name><t:ContentType>application/pdf</t:ContentType>\
           <t:LastModifiedTime>2020-01-01T00:00:00</t:LastModifiedTime>\
         </t:FileAttachment></t:Attachments>\
         <t:LastModifiedTime>2025-06-01T00:00:00Z</t:LastModifiedTime>\
         <t:MyResponseType>Accept</t:MyResponseType>\
         </t:CalendarItem>"
    );
    let parsed = CalendarItemRaw::parse(&body).unwrap();
    assert_eq!(parsed.sensitivity.as_deref(), Some("Private"));
    assert_eq!(parsed.my_response_type.as_deref(), Some("Accept"));
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].attachment_id, "ATT1");
    assert_eq!(
        parsed.last_modified.as_deref(),
        Some("2025-06-01T00:00:00Z")
    );
}

#[test]
fn calendar_parses_reminder_resources_and_online_meeting() {
    let body = format!(
        "<t:CalendarItem{NS}>\
         <t:ItemId Id=\"C1\" ChangeKey=\"K1\"/>\
         <t:Subject>Standup</t:Subject>\
         <t:UID>uid-r</t:UID>\
         <t:ReminderIsSet>true</t:ReminderIsSet>\
         <t:ReminderMinutesBeforeStart>10</t:ReminderMinutesBeforeStart>\
         <t:Start>2025-06-15T14:00:00Z</t:Start>\
         <t:End>2025-06-15T15:00:00Z</t:End>\
         <t:IsOnlineMeeting>true</t:IsOnlineMeeting>\
         <t:NetShowUrl>https://meet/abc</t:NetShowUrl>\
         <t:RequiredAttendees>\
           <t:Attendee><t:Mailbox><t:Name>Bob</t:Name><t:EmailAddress>bob@x</t:EmailAddress></t:Mailbox><t:ResponseType>Accept</t:ResponseType></t:Attendee>\
         </t:RequiredAttendees>\
         <t:Resources>\
           <t:Attendee><t:Mailbox><t:Name>Room 7</t:Name><t:EmailAddress>room7@x</t:EmailAddress></t:Mailbox></t:Attendee>\
         </t:Resources>\
         </t:CalendarItem>"
    );
    let parsed = CalendarItemRaw::parse(&body).unwrap();
    assert_eq!(parsed.reminder_is_set, Some(true));
    assert_eq!(parsed.reminder_minutes_before_start, Some(10));
    assert_eq!(parsed.is_online_meeting, Some(true));
    assert_eq!(parsed.net_show_url.as_deref(), Some("https://meet/abc"));
    assert_eq!(parsed.required_attendees.len(), 1);
    assert_eq!(parsed.required_attendees[0].email.as_deref(), Some("bob@x"));
    assert_eq!(parsed.resources.len(), 1);
    assert_eq!(parsed.resources[0].email.as_deref(), Some("room7@x"));
}

#[test]
fn distribution_list_parses_members() {
    let body = format!(
        "<t:DistributionList{NS}>\
         <t:ItemId Id=\"DL1\" ChangeKey=\"K1\"/>\
         <t:DisplayName>The Team</t:DisplayName>\
         <t:Members>\
           <t:Member><t:Mailbox><t:Name>Bob</t:Name><t:EmailAddress>bob@x</t:EmailAddress></t:Mailbox><t:Status>Normal</t:Status></t:Member>\
           <t:Member><t:Mailbox><t:Name>Carol</t:Name><t:EmailAddress>carol@x</t:EmailAddress></t:Mailbox></t:Member>\
         </t:Members>\
         </t:DistributionList>"
    );
    let parsed = ContactItemRaw::parse(&body).unwrap();
    assert!(parsed.is_group);
    assert_eq!(parsed.id.id, "DL1");
    assert_eq!(parsed.display_name.as_deref(), Some("The Team"));
    assert_eq!(parsed.members.len(), 2);
    assert_eq!(parsed.members[0].email.as_deref(), Some("bob@x"));
    assert_eq!(parsed.members[1].name.as_deref(), Some("Carol"));
}

#[test]
fn contact_decodes_xml_entities() {
    let body = format!(
        "<t:Contact{NS}>\
         <t:ItemId Id=\"C1\" ChangeKey=\"K1\"/>\
         <t:Categories><t:String>R&amp;D</t:String></t:Categories>\
         <t:DisplayName>Sales &amp; Marketing &lt;Team&gt;</t:DisplayName>\
         <t:CompanyName>AT&amp;T</t:CompanyName>\
         <t:Department>R&amp;D</t:Department>\
         <t:EmailAddresses><t:Entry Key=\"EmailAddress1\">a&amp;b@x.com</t:Entry></t:EmailAddresses>\
         </t:Contact>"
    );
    let parsed = ContactItemRaw::parse(&body).unwrap();
    assert_eq!(
        parsed.display_name.as_deref(),
        Some("Sales & Marketing <Team>")
    );
    assert_eq!(parsed.company_name.as_deref(), Some("AT&T"));
    assert_eq!(parsed.department.as_deref(), Some("R&D"));
    assert_eq!(parsed.categories, vec!["R&D".to_owned()]);
    assert_eq!(parsed.emails[0].1, "a&b@x.com");
}

#[test]
fn calendar_decodes_xml_entities_in_subject_and_location() {
    let body = format!(
        "<t:CalendarItem{NS}>\
         <t:ItemId Id=\"E1\" ChangeKey=\"K\"/>\
         <t:Subject>Q&amp;A &lt;all hands&gt;</t:Subject>\
         <t:Location>Room A &amp; B</t:Location>\
         <t:Start>2025-06-15T14:00:00Z</t:Start>\
         <t:End>2025-06-15T15:00:00Z</t:End>\
         </t:CalendarItem>"
    );
    let parsed = CalendarItemRaw::parse(&body).unwrap();
    assert_eq!(parsed.subject.as_deref(), Some("Q&A <all hands>"));
    assert_eq!(parsed.location.as_deref(), Some("Room A & B"));
}

#[test]
fn folder_decodes_xml_entities_in_display_name() {
    let body = format!(
        "<t:Folder{NS}><t:FolderId Id=\"F1\" ChangeKey=\"K\"/>\
         <t:DisplayName>Sales &amp; Marketing</t:DisplayName>\
         <t:FolderClass>IPF.Note</t:FolderClass></t:Folder>"
    );
    let entry = FolderEntry::parse_inner(&body).unwrap().unwrap();
    assert_eq!(entry.display_name, "Sales & Marketing");
}

#[test]
fn response_capture_preserves_entities_for_reparse() {
    let body = format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages><m:GetItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode><m:Items>\
         <t:Contact><t:ItemId Id=\"C1\" ChangeKey=\"K\"/><t:CompanyName>AT&amp;T</t:CompanyName></t:Contact>\
         </m:Items></m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
    );
    let msgs = ResponseMessage::parse_all(body.as_bytes(), "GetItemResponseMessage").unwrap();
    assert!(
        msgs[0].inner_xml.contains("AT&amp;T") || msgs[0].inner_xml.contains("AT&T"),
        "capture must preserve the entity for the per-item parser: {}",
        msgs[0].inner_xml
    );
    let parsed = ContactItemRaw::parse(&msgs[0].inner_xml).unwrap();
    assert_eq!(parsed.company_name.as_deref(), Some("AT&T"));
}

#[test]
fn contact_parses_emails_phones_addresses() {
    let body = format!(
        "<t:Contact{NS}>\
         <t:ItemId Id=\"C1\" ChangeKey=\"K1\"/>\
         <t:DisplayName>Alice Doe</t:DisplayName>\
         <t:GivenName>Alice</t:GivenName>\
         <t:Surname>Doe</t:Surname>\
         <t:EmailAddresses>\
           <t:Entry Key=\"EmailAddress1\">alice@example.com</t:Entry>\
         </t:EmailAddresses>\
         <t:PhoneNumbers>\
           <t:Entry Key=\"BusinessPhone1\">+15551234</t:Entry>\
         </t:PhoneNumbers>\
         <t:PhysicalAddresses>\
           <t:Entry Key=\"Home\"><t:Street>1 Main</t:Street><t:City>Town</t:City><t:CountryOrRegion>US</t:CountryOrRegion></t:Entry>\
         </t:PhysicalAddresses>\
         </t:Contact>"
    );
    let parsed = ContactItemRaw::parse(&body).unwrap();
    assert_eq!(parsed.given_name.as_deref(), Some("Alice"));
    assert_eq!(parsed.surname.as_deref(), Some("Doe"));
    assert_eq!(parsed.emails.len(), 1);
    assert_eq!(parsed.emails[0].0, "EmailAddress1");
    assert_eq!(parsed.emails[0].1, "alice@example.com");
    assert_eq!(parsed.phones.len(), 1);
    assert_eq!(parsed.phones[0].0, "BusinessPhone1");
    assert_eq!(parsed.addresses.len(), 1);
    assert_eq!(parsed.addresses[0].key, "Home");
    assert_eq!(parsed.addresses[0].street.as_deref(), Some("1 Main"));
    assert_eq!(parsed.addresses[0].country.as_deref(), Some("US"));
}

#[test]
fn get_item_with_unprefixed_types_elements_parses() {
    let body = format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages>\
         <m:GetItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Items xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
         <Message><ItemId Id=\"NP1\" ChangeKey=\"K\"/>\
         <Subject>Hello</Subject>\
         <MimeContent CharacterSet=\"UTF-8\">SGk=</MimeContent>\
         </Message></m:Items></m:GetItemResponseMessage>\
         </m:ResponseMessages></m:GetItemResponse>"
    );
    let msgs = ResponseMessage::parse_all(body.as_bytes(), "GetItemResponseMessage").unwrap();
    assert_eq!(msgs.len(), 1);
    let item = MessageItem::parse(&msgs[0].inner_xml).unwrap();
    assert_eq!(item.id.id, "NP1");
    assert_eq!(item.subject.as_deref(), Some("Hello"));
    assert_eq!(item.mime_content.as_deref(), Some("SGk="));
}

#[test]
fn default_namespace_response_parses_via_resolved_names() {
    let body = "<Envelope xmlns=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
                xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
                <Body>\
                <m:FindFolderResponse><m:ResponseMessages>\
                <m:FindFolderResponseMessage ResponseClass=\"Success\">\
                <m:ResponseCode>NoError</m:ResponseCode>\
                <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\">\
                <t:Folders><t:Folder><t:FolderId Id=\"D1\"/><t:ParentFolderId Id=\"R\"/>\
                <t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Inbox</t:DisplayName>\
                </t:Folder></t:Folders></m:RootFolder>\
                </m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>\
                </Body></Envelope>";
    let parsed = FindFolderResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(parsed.folders.len(), 1);
    assert_eq!(parsed.folders[0].folder_id.id, "D1");
    assert_eq!(parsed.folders[0].display_name, "Inbox");
}

#[test]
fn fault_response_code_in_messages_namespace_is_recognised() {
    let body = format!(
        "<soap:Envelope{NS}><soap:Body><soap:Fault>\
         <faultcode>soap:Server</faultcode><faultstring>busy</faultstring>\
         <detail><m:ResponseCode>ErrorServerBusy</m:ResponseCode>\
         <t:MessageXml><t:Value Name=\"BackOffMilliseconds\">42</t:Value></t:MessageXml>\
         </detail></soap:Fault></soap:Body></soap:Envelope>"
    );
    let env = EnvelopeKind::read(body.as_bytes()).unwrap();
    match env {
        EnvelopeKind::Fault { fault, .. } => {
            assert!(matches!(
                fault.response_code,
                ResponseCode::ServerBusy { .. }
            ));
            assert_eq!(fault.back_off_ms, Some(42));
        }
        _ => panic!("expected fault"),
    }
}

#[test]
fn change_key_attribute_with_entities_is_xml_decoded() {
    let body = format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\">\
         <t:Folders><t:Folder><t:FolderId Id=\"FID\" ChangeKey=\"A&amp;B\"/>\
         <t:DisplayName>X</t:DisplayName></t:Folder></t:Folders>\
         </m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    );
    let parsed = FindFolderResponse::parse(body.as_bytes()).unwrap();
    assert_eq!(parsed.folders[0].folder_id.change_key, "A&B");
}

#[test]
fn warning_response_class_is_treated_as_success() {
    let body = format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages>\
         <m:GetItemResponseMessage ResponseClass=\"Warning\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Items><t:Message><t:ItemId Id=\"W1\" ChangeKey=\"K\"/></t:Message></m:Items>\
         </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
    );
    let r = ResponseMessage::parse_all(body.as_bytes(), "GetItemResponseMessage").unwrap();
    assert_eq!(r.len(), 1);
    assert!(r[0].success, "Warning should be success-equivalent");
}

#[test]
fn get_folder_response_message_captures_folder_inner() {
    let body = format!(
        "<m:GetFolderResponse{NS}><m:ResponseMessages>\
         <m:GetFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Folders><t:Folder><t:FolderId Id=\"F1\"/>\
         <t:DisplayName>Inbox</t:DisplayName></t:Folder></m:Folders>\
         </m:GetFolderResponseMessage>\
         <m:GetFolderResponseMessage ResponseClass=\"Error\">\
         <m:ResponseCode>ErrorAccessDenied</m:ResponseCode></m:GetFolderResponseMessage>\
         </m:ResponseMessages></m:GetFolderResponse>"
    );
    let msgs = ResponseMessage::parse_all(body.as_bytes(), "GetFolderResponseMessage").unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(msgs[0].success);
    let f = FolderEntry::parse_inner(&msgs[0].inner_xml)
        .unwrap()
        .unwrap();
    assert_eq!(f.folder_id.id, "F1");
    assert!(!msgs[1].success);
}

#[test]
fn get_attachment_inline_decodes_content_field() {
    let body = format!(
        "<m:GetAttachmentResponse{NS}><m:ResponseMessages>\
         <m:GetAttachmentResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Attachments>\
           <t:FileAttachment><t:AttachmentId Id=\"A1\"/><t:Name>p.png</t:Name>\
           <t:ContentType>image/png</t:ContentType>\
           <t:IsContactPhoto>true</t:IsContactPhoto>\
           <t:Content>iVBORw0KGgo=</t:Content></t:FileAttachment>\
         </m:Attachments></m:GetAttachmentResponseMessage></m:ResponseMessages></m:GetAttachmentResponse>"
    );
    let items = GetAttachmentInline::parse_all(body.as_bytes()).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].attachment_id, "A1");
    assert_eq!(items[0].name.as_deref(), Some("p.png"));
    assert_eq!(items[0].content_type.as_deref(), Some("image/png"));
    assert!(items[0].is_contact_photo);
    assert_eq!(items[0].content_base64, "iVBORw0KGgo=");
}
