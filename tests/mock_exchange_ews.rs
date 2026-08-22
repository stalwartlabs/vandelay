/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use mockito::Matcher;
use vandelay::exchange_ews::EwsClient;
use vandelay::exchange_ews::autodiscover::{DiscoverySource, discover};
use vandelay::exchange_ews::error::EwsError;
use vandelay::exchange_ews::parse::{
    EnvelopeKind, parse_find_folder_response, parse_find_item_response,
    parse_get_attachment_inline, parse_response_messages, parse_sync_folder_items_response,
    read_envelope_summary,
};
use vandelay::exchange_ews::types::{FolderClass, FolderId, ItemId, ResponseCode, ServerVersion};
use vandelay::exchange_ews::xml::{
    FolderRef, ItemShape, Traversal, find_folder_body, find_item_body, get_attachment_body,
    get_item_body, sync_folder_items_body,
};
use vandelay::jmap::http::{Auth, RetryPolicy};

const TXT_XML: &str = "text/xml; charset=utf-8";
const APP_JSON: &str = "application/json";

fn client(retries: u32) -> EwsClient {
    EwsClient::new(
        Auth::Bearer {
            token: "t".to_owned(),
        },
        RetryPolicy::new(retries),
        false,
    )
}

const NS: &str = " xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                  xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
                  xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\"";

fn envelope(inner: &str) -> String {
    format!(
        "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"1\"/></soap:Header><soap:Body>{inner}</soap:Body></soap:Envelope>"
    )
}

#[test]
fn autodiscover_v2_returns_global_endpoint() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/autodiscover/autodiscover.json")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("Email".into(), "alice@contoso.com".into()),
            Matcher::UrlEncoded("Protocol".into(), "Ews".into()),
        ]))
        .with_status(200)
        .with_header("content-type", APP_JSON)
        .with_body(r#"{"Protocol":"Ews","Url":"https://outlook.office365.com/EWS/Exchange.asmx"}"#)
        .create();
    let _ = server;
    let url = "https://outlook.office365.com/EWS/Exchange.asmx";
    assert!(vandelay::exchange_ews::autodiscover::is_fully_qualified_ews_url(url));
    let r = discover(Some(url), None, None, false).unwrap();
    assert_eq!(r.source, DiscoverySource::SuppliedUrl);
    assert_eq!(r.ews_url, url);
}

#[test]
fn find_folder_pagination_and_classification() {
    let body = format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"3\" IncludesLastItemInRange=\"true\">\
         <t:Folders>\
         <t:Folder><t:FolderId Id=\"FMAIL\" ChangeKey=\"C\"/><t:ParentFolderId Id=\"ROOT\"/>\
           <t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Inbox</t:DisplayName></t:Folder>\
         <t:CalendarFolder><t:FolderId Id=\"FCAL\"/><t:ParentFolderId Id=\"ROOT\"/>\
           <t:FolderClass>IPF.Appointment</t:FolderClass><t:DisplayName>Calendar</t:DisplayName></t:CalendarFolder>\
         <t:ContactsFolder><t:FolderId Id=\"FCON\"/><t:ParentFolderId Id=\"ROOT\"/>\
           <t:FolderClass>IPF.Contact</t:FolderClass><t:DisplayName>Contacts</t:DisplayName></t:ContactsFolder>\
         </t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    );
    let parsed = parse_find_folder_response(envelope(&body).as_bytes()).unwrap();
    assert_eq!(parsed.folders.len(), 3);
    let inbox = &parsed.folders[0];
    assert_eq!(inbox.folder_id.id, "FMAIL");
    assert_eq!(inbox.folder_class, "IPF.Note");
}

#[test]
fn find_folder_classifies_workmail_folder_class_set() {
    let body = format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"7\" IncludesLastItemInRange=\"true\"><t:Folders>\
         <t:Folder><t:FolderId Id=\"INBOX\"/><t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>Inbox</t:DisplayName></t:Folder>\
         <t:Folder><t:FolderId Id=\"CUSTOM\"/><t:DisplayName>SyncFolder</t:DisplayName></t:Folder>\
         <t:Folder><t:FolderId Id=\"RSS\"/><t:FolderClass>IPF.Note.OutlookHomepage</t:FolderClass><t:DisplayName>RSS Feeds</t:DisplayName></t:Folder>\
         <t:Folder><t:FolderId Id=\"CFG\"/><t:FolderClass>IPF.Configuration</t:FolderClass><t:DisplayName>Quick Step Settings</t:DisplayName></t:Folder>\
         <t:TasksFolder><t:FolderId Id=\"TASKS\"/><t:FolderClass>IPF.Task</t:FolderClass><t:DisplayName>Tasks</t:DisplayName></t:TasksFolder>\
         <t:CalendarFolder><t:FolderId Id=\"CAL\"/><t:FolderClass>IPF.Appointment</t:FolderClass><t:DisplayName>Calendar</t:DisplayName></t:CalendarFolder>\
         <t:ContactsFolder><t:FolderId Id=\"CON\"/><t:FolderClass>IPF.Contact</t:FolderClass><t:DisplayName>Contacts</t:DisplayName></t:ContactsFolder>\
         </t:Folders></m:RootFolder></m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    );
    let parsed = parse_find_folder_response(envelope(&body).as_bytes()).unwrap();
    let by_id = |id: &str| {
        parsed
            .folders
            .iter()
            .find(|f| f.folder_id.id == id)
            .map(|f| FolderClass::from_ipf(&f.folder_class))
            .unwrap()
    };
    assert_eq!(by_id("INBOX"), FolderClass::Mail);
    assert_eq!(
        by_id("CUSTOM"),
        FolderClass::Mail,
        "a user folder with no FolderClass must import as mail, never be dropped"
    );
    assert_eq!(
        by_id("RSS"),
        FolderClass::Skipped,
        "RSS Feeds (IPF.Note.OutlookHomepage) holds feed posts, not mail"
    );
    assert_eq!(by_id("CFG"), FolderClass::Skipped);
    assert_eq!(by_id("TASKS"), FolderClass::Skipped);
    assert_eq!(by_id("CAL"), FolderClass::Calendar);
    assert_eq!(by_id("CON"), FolderClass::Contacts);
}

#[test]
fn find_item_distribution_list_does_not_clobber_preceding_contact() {
    let body = envelope(&format!(
        "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"3\" IncludesLastItemInRange=\"true\"><t:Items>\
         <t:Contact><t:ItemId Id=\"C1\" ChangeKey=\"k1\"/></t:Contact>\
         <t:Contact><t:ItemId Id=\"C2\" ChangeKey=\"k2\"/></t:Contact>\
         <t:DistributionList><t:ItemId Id=\"DL1\" ChangeKey=\"k3\"/></t:DistributionList>\
         </t:Items></m:RootFolder></m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
    ));
    let parsed = parse_find_item_response(body.as_bytes()).unwrap();
    assert_eq!(
        parsed.items.len(),
        3,
        "DistributionList must be its own item, not clobber C2"
    );
    let ids: Vec<&str> = parsed.items.iter().map(|i| i.id.id.as_str()).collect();
    assert_eq!(ids, ["C1", "C2", "DL1"]);
    assert_eq!(
        parsed.items[2].element.to_ascii_lowercase(),
        "distributionlist"
    );
}

#[test]
fn find_item_offset_loop_terminates_on_includes_last_true() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());

    let page1 = envelope(&format!(
        "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"4\" IncludesLastItemInRange=\"false\">\
         <t:Items><t:Message><t:ItemId Id=\"A\" ChangeKey=\"1\"/></t:Message>\
         <t:Message><t:ItemId Id=\"B\" ChangeKey=\"1\"/></t:Message></t:Items></m:RootFolder>\
         </m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
    ));
    let page2 = envelope(&format!(
        "<m:FindItemResponse{NS}><m:ResponseMessages><m:FindItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"4\" IncludesLastItemInRange=\"true\">\
         <t:Items><t:Message><t:ItemId Id=\"C\" ChangeKey=\"1\"/></t:Message>\
         <t:Message><t:ItemId Id=\"D\" ChangeKey=\"1\"/></t:Message></t:Items></m:RootFolder>\
         </m:FindItemResponseMessage></m:ResponseMessages></m:FindItemResponse>"
    ));

    let _m1 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .match_body(Matcher::Regex("Offset=\"0\"".into()))
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(page1)
        .create();
    let _m2 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .match_body(Matcher::Regex("Offset=\"2\"".into()))
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(page2)
        .create();

    let c = client(0);
    let folder = FolderId::new("FID", "FCK");
    let body0 = find_item_body(FolderRef::Concrete(&folder), Traversal::Shallow, 0, 2);
    let r0 = c.call(&url, "FindItem", &body0).unwrap();
    let parsed0 = parse_find_item_response(&r0.body).unwrap();
    assert_eq!(parsed0.items.len(), 2);
    assert!(parsed0.more);

    let body1 = find_item_body(FolderRef::Concrete(&folder), Traversal::Shallow, 2, 2);
    let r1 = c.call(&url, "FindItem", &body1).unwrap();
    let parsed1 = parse_find_item_response(&r1.body).unwrap();
    assert_eq!(parsed1.items.len(), 2);
    assert!(!parsed1.more);
}

#[test]
fn get_item_mixed_success_and_per_item_error() {
    let body = envelope(&format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages>\
         <m:GetItemResponseMessage ResponseClass=\"Success\">\
           <m:ResponseCode>NoError</m:ResponseCode>\
           <m:Items><t:Message><t:ItemId Id=\"OK1\" ChangeKey=\"K\"/>\
             <t:Subject>S</t:Subject>\
             <t:MimeContent CharacterSet=\"UTF-8\">SGVsbG8=</t:MimeContent>\
             </t:Message></m:Items></m:GetItemResponseMessage>\
         <m:GetItemResponseMessage ResponseClass=\"Error\">\
           <m:ResponseCode>ErrorItemNotFound</m:ResponseCode>\
           <m:MessageText>not found</m:MessageText>\
           </m:GetItemResponseMessage>\
         </m:ResponseMessages></m:GetItemResponse>"
    ));
    let r = parse_response_messages(body.as_bytes(), b"GetItemResponseMessage").unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].success);
    assert!(r[0].inner_xml.contains("SGVsbG8="));
    assert!(!r[1].success);
    assert!(matches!(r[1].response_code, ResponseCode::ItemNotFound));
}

#[test]
fn server_busy_with_back_off_triggers_retry_and_eventually_succeeds() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let busy_body = format!(
        "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"1\"/></soap:Header>\
         <soap:Body><soap:Fault><faultcode>soap:Server</faultcode><faultstring>busy</faultstring><detail>\
         <ResponseCode xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">ErrorServerBusy</ResponseCode>\
         <t:MessageXml><t:Value Name=\"BackOffMilliseconds\">200</t:Value></t:MessageXml>\
         </detail></soap:Fault></soap:Body></soap:Envelope>"
    );
    let ok_body = envelope(&format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"0\" IncludesLastItemInRange=\"true\"><t:Folders/></m:RootFolder>\
         </m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    ));

    let _m1 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(busy_body)
        .expect(1)
        .create();
    let _m2 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(ok_body)
        .expect_at_least(1)
        .create();

    let c = client(3);
    let body = find_folder_body(
        FolderRef::Distinguished(
            vandelay::exchange_ews::types::DistinguishedFolderId::MsgFolderRoot,
        ),
        Traversal::Deep,
    );
    let r = c
        .call(&url, "FindFolder", &body)
        .expect("call should retry past busy fault");
    let parsed = parse_find_folder_response(&r.body).unwrap();
    assert!(parsed.folders.is_empty());
    assert!(c.retries_observed() >= 1);
}

#[test]
fn http_401_surfaces_as_auth_error() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let _m = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(401)
        .with_header("www-authenticate", "Bearer error=\"invalid_token\"")
        .with_body("unauthorized")
        .create();
    let c = client(0);
    let body = find_folder_body(
        FolderRef::Distinguished(
            vandelay::exchange_ews::types::DistinguishedFolderId::MsgFolderRoot,
        ),
        Traversal::Deep,
    );
    let err = c.call(&url, "FindFolder", &body).unwrap_err();
    assert!(matches!(err, EwsError::Auth(_)), "got {err:?}");
}

#[test]
fn mime_content_round_trips_through_base64_decode() {
    let original = b"From: alice@x\r\nSubject: hi\r\n\r\nbody";
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(original);
    let body = envelope(&format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages>\
         <m:GetItemResponseMessage ResponseClass=\"Success\">\
           <m:ResponseCode>NoError</m:ResponseCode>\
           <m:Items><t:Message><t:ItemId Id=\"X\" ChangeKey=\"K\"/>\
             <t:Subject>hi</t:Subject>\
             <t:MimeContent CharacterSet=\"UTF-8\">{encoded}</t:MimeContent>\
             </t:Message></m:Items></m:GetItemResponseMessage>\
         </m:ResponseMessages></m:GetItemResponse>"
    ));
    let r = parse_response_messages(body.as_bytes(), b"GetItemResponseMessage").unwrap();
    let item = vandelay::exchange_ews::parse::parse_message_item(&r[0].inner_xml).unwrap();
    let s = item.mime_content.unwrap();
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .unwrap();
    assert_eq!(bytes, original);
}

#[test]
fn get_attachment_inline_decodes_photo_blob() {
    let body = envelope(&format!(
        "<m:GetAttachmentResponse{NS}><m:ResponseMessages>\
         <m:GetAttachmentResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Attachments>\
           <t:FileAttachment><t:AttachmentId Id=\"A1\"/><t:Name>p.png</t:Name>\
           <t:ContentType>image/png</t:ContentType>\
           <t:IsContactPhoto>true</t:IsContactPhoto>\
           <t:Content>iVBORw0KGgo=</t:Content></t:FileAttachment>\
         </m:Attachments></m:GetAttachmentResponseMessage></m:ResponseMessages></m:GetAttachmentResponse>"
    ));
    let items = parse_get_attachment_inline(body.as_bytes()).unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].is_contact_photo);
    assert_eq!(items[0].content_base64, "iVBORw0KGgo=");
    assert_eq!(items[0].content_type.as_deref(), Some("image/png"));
}

#[test]
fn calendar_item_master_inlines_modified_and_deleted_occurrences() {
    let body = "<vandelay-inner xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
         <t:CalendarItem>\
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
         </t:CalendarItem></vandelay-inner>";
    let item = vandelay::exchange_ews::parse::parse_calendar_item(body).unwrap();
    assert_eq!(item.id.id, "M1");
    assert_eq!(item.uid.as_deref(), Some("uid-1"));
    assert_eq!(item.modified_occurrences.len(), 1);
    assert_eq!(
        item.modified_occurrences[0].item_id.id, "OCC1",
        "occurrence ItemId must populate the occurrence, not overwrite the master"
    );
    assert_eq!(item.deleted_occurrences.len(), 1);
}

#[test]
fn organizer_and_attendee_addresses_survive_an_ex_routing_type() {
    let body = "<vandelay-inner xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
         <t:CalendarItem>\
         <t:ItemId Id=\"M3\" ChangeKey=\"K1\"/>\
         <t:Subject>Review</t:Subject>\
         <t:UID>uid-3</t:UID>\
         <t:Start>2025-06-15T14:00:00Z</t:Start>\
         <t:End>2025-06-15T15:00:00Z</t:End>\
         <t:Organizer><t:Mailbox><t:Name>Alice</t:Name>\
           <t:EmailAddress>alice@example.com</t:EmailAddress>\
           <t:RoutingType>EX</t:RoutingType><t:MailboxType>Mailbox</t:MailboxType></t:Mailbox></t:Organizer>\
         <t:RequiredAttendees><t:Attendee><t:Mailbox><t:Name>Kristina Morgental</t:Name>\
           <t:EmailAddress>/o=ExchangeLabs/ou=Exchange Administrative Group (FYDIBOHF23SPDLT)/cn=Recipients/cn=bdc77b18152647a29d28ce1188376dc9-kristina</t:EmailAddress>\
           <t:RoutingType>EX</t:RoutingType></t:Mailbox><t:ResponseType>Unknown</t:ResponseType></t:Attendee></t:RequiredAttendees>\
         </t:CalendarItem></vandelay-inner>";
    let item = vandelay::exchange_ews::parse::parse_calendar_item(body).unwrap();
    let event = vandelay::exchange_ews::calendar_map::to_jscalendar(&item);

    assert_eq!(
        event.data["organizerCalendarAddress"], "mailto:alice@example.com",
        "a usable address must be kept even when RoutingType says EX"
    );
    let participants = event.data["participants"].as_object().unwrap();
    let organizer = participants
        .values()
        .find(|p| p["calendarAddress"] == "mailto:alice@example.com")
        .expect("organizer participant");
    assert_eq!(organizer["email"], "alice@example.com");

    let attendee = participants
        .values()
        .find(|p| p["name"] == "Kristina Morgental")
        .expect("attendee participant");
    assert!(
        attendee["calendarAddress"]
            .as_str()
            .unwrap()
            .starts_with("urn:x-vandelay:attendee:"),
        "a legacy directory reference cannot be resolved and stays synthetic"
    );
    assert!(
        attendee.get("email").is_none(),
        "no email is invented for a directory reference"
    );
}

#[test]
fn recurrence_end_date_with_a_timezone_offset_yields_a_bounded_series() {
    let body = "<vandelay-inner xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
         <t:CalendarItem>\
         <t:ItemId Id=\"M2\" ChangeKey=\"K1\"/>\
         <t:Subject>Biweekly</t:Subject>\
         <t:UID>uid-2</t:UID>\
         <t:Start>2021-08-04T21:15:00Z</t:Start>\
         <t:End>2021-08-04T22:15:00Z</t:End>\
         <t:CalendarItemType>RecurringMaster</t:CalendarItemType>\
         <t:Recurrence>\
           <t:WeeklyRecurrence><t:Interval>2</t:Interval><t:DaysOfWeek>Wednesday</t:DaysOfWeek></t:WeeklyRecurrence>\
           <t:EndDateRecurrence><t:StartDate>2021-08-04-06:00</t:StartDate><t:EndDate>2021-09-30-06:00</t:EndDate></t:EndDateRecurrence>\
         </t:Recurrence>\
         </t:CalendarItem></vandelay-inner>";
    let item = vandelay::exchange_ews::parse::parse_calendar_item(body).unwrap();
    let event = vandelay::exchange_ews::calendar_map::to_jscalendar(&item);
    let rule = &event.data["recurrenceRule"];
    assert_eq!(rule["frequency"], "weekly");
    assert_eq!(
        rule["until"], "2021-09-30T23:59:59",
        "an EndDate carrying a UTC offset must still produce a valid LocalDateTime"
    );
}

#[test]
fn sync_folder_items_creates_updates_deletes_round_trip() {
    let body = envelope(&format!(
        "<m:SyncFolderItemsResponse{NS}><m:ResponseMessages><m:SyncFolderItemsResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:SyncState>STATE-1</m:SyncState>\
         <m:IncludesLastItemInRange>true</m:IncludesLastItemInRange>\
         <m:Changes>\
           <t:Create><t:Message><t:ItemId Id=\"N1\" ChangeKey=\"A\"/></t:Message></t:Create>\
           <t:Create><t:Message><t:ItemId Id=\"N2\" ChangeKey=\"A\"/></t:Message></t:Create>\
           <t:Update><t:Message><t:ItemId Id=\"U1\" ChangeKey=\"B\"/></t:Message></t:Update>\
           <t:Delete><t:ItemId Id=\"D1\"/></t:Delete>\
           <t:ReadFlagChange><t:ItemId Id=\"R1\"/><t:IsRead>true</t:IsRead></t:ReadFlagChange>\
         </m:Changes></m:SyncFolderItemsResponseMessage></m:ResponseMessages></m:SyncFolderItemsResponse>"
    ));
    let parsed = parse_sync_folder_items_response(body.as_bytes()).unwrap();
    assert_eq!(parsed.sync_state, "STATE-1");
    assert!(!parsed.more);
    assert_eq!(parsed.changes.len(), 5);
}

#[test]
fn invalid_sync_state_data_fault_is_surfaced() {
    let body = format!(
        "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"15\" MinorVersion=\"1\"/></soap:Header>\
         <soap:Body><soap:Fault><faultcode>soap:Server</faultcode><faultstring>bad state</faultstring><detail>\
         <ResponseCode xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">ErrorInvalidSyncStateData</ResponseCode>\
         </detail></soap:Fault></soap:Body></soap:Envelope>"
    );
    let env = read_envelope_summary(body.as_bytes()).unwrap();
    match env {
        EnvelopeKind::Fault { fault, .. } => {
            assert!(matches!(
                fault.response_code,
                ResponseCode::InvalidSyncStateData
            ));
        }
        _ => panic!("expected fault"),
    }
}

#[test]
fn coordinator_diff_classifies_new_vanished_changed_unchanged() {
    use vandelay::db::exchange_ews_ids::ItemRow;
    use vandelay::sync::import_exchange_ews::items::{EnumeratedItem, diff};

    let server = vec![
        EnumeratedItem {
            element: "Message".into(),
            id: ItemId::new("A", "ck-1"),
        },
        EnumeratedItem {
            element: "Message".into(),
            id: ItemId::new("B", "ck-2"),
        },
        EnumeratedItem {
            element: "Message".into(),
            id: ItemId::new("C", "ck-1"),
        },
    ];
    let local = vec![
        ItemRow {
            item_id: "A".into(),
            change_key: "ck-1".into(),
            local_id: 1,
        },
        ItemRow {
            item_id: "B".into(),
            change_key: "ck-1".into(),
            local_id: 2,
        },
        ItemRow {
            item_id: "Z".into(),
            change_key: "ck-9".into(),
            local_id: 99,
        },
    ];
    let plan = diff(&server, &local);
    assert_eq!(plan.new.len(), 1);
    assert_eq!(plan.new[0].id, "C");
    assert_eq!(plan.present_unchanged.len(), 1);
    assert_eq!(plan.present_changed.len(), 1);
    assert_eq!(plan.present_changed[0].0.id, "B");
    assert_eq!(plan.vanished.len(), 1);
    assert_eq!(plan.vanished[0].0, "Z");
}

#[test]
fn source_change_protection_refuses_different_account() {
    use rusqlite::Connection;
    use vandelay::db;
    use vandelay::db::sources::SourceKey;

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(tmp.path()).unwrap();
    db::init::apply_schema(&conn).unwrap();
    let key = SourceKey {
        kind: "exchange_ews".into(),
        session_url: "https://outlook.office365.com/EWS/Exchange.asmx".into(),
        account_id: "alice@contoso.com".into(),
    };
    db::sources::upsert_source(&conn, &key, None, "alice@contoso.com").unwrap();
    let conflict = db::sources::conflicting_source(
        &conn,
        "exchange_ews",
        "https://outlook.office365.com/EWS/Exchange.asmx",
        "bob@contoso.com",
    )
    .unwrap();
    assert!(conflict.is_some(), "different account_id must conflict");
}

#[test]
fn mailbox_kinds_are_three_separate_sources() {
    use rusqlite::Connection;
    use vandelay::db;
    use vandelay::db::sources::SourceKey;

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(tmp.path()).unwrap();
    db::init::apply_schema(&conn).unwrap();
    let primary = SourceKey {
        kind: "exchange_ews".into(),
        session_url: "https://x/EWS/Exchange.asmx".into(),
        account_id: "alice@contoso.com".into(),
    };
    let archive = SourceKey {
        kind: "exchange_ews".into(),
        session_url: "https://x/EWS/Exchange.asmx".into(),
        account_id: "alice@contoso.com#archive".into(),
    };
    let public = SourceKey {
        kind: "exchange_ews".into(),
        session_url: "https://x/EWS/Exchange.asmx".into(),
        account_id: "__public_folders__@contoso.com".into(),
    };
    let p = db::sources::upsert_source(&conn, &primary, None, "alice@contoso.com").unwrap();
    let a = db::sources::upsert_source(&conn, &archive, None, "alice@contoso.com").unwrap();
    let q = db::sources::upsert_source(&conn, &public, None, "alice@contoso.com").unwrap();
    assert_ne!(p, a);
    assert_ne!(p, q);
    assert_ne!(a, q);
}

#[test]
fn get_item_batches_chunk_the_id_list() {
    let ids: Vec<ItemId> = (0..7).map(|i| ItemId::new(format!("I{i}"), "K")).collect();
    let chunks: Vec<&[ItemId]> = ids.chunks(3).collect();
    assert_eq!(chunks.len(), 3);
    for chunk in &chunks {
        let body = get_item_body(ItemShape::Message, chunk, ServerVersion::Exchange2013Sp1);
        assert!(body.contains("<m:GetItem>"));
        for id in *chunk {
            assert!(body.contains(&format!("Id=\"{}\"", id.id)));
        }
    }
}

#[test]
fn sync_folder_items_request_body_carries_state_and_max() {
    let folder = FolderId::new("FID", "FCK");
    let body = sync_folder_items_body(&folder, "OPAQUE", 512, ServerVersion::Exchange2013Sp1);
    assert!(body.contains("<m:SyncState>OPAQUE</m:SyncState>"));
    assert!(body.contains("<m:MaxChangesReturned>512</m:MaxChangesReturned>"));
    assert!(body.contains("<m:SyncScope>NormalItems</m:SyncScope>"));
    assert!(body.contains("<m:SyncFolderId><t:FolderId Id=\"FID\" ChangeKey=\"FCK\"/>"));
}

#[test]
fn get_attachment_body_carries_inline_request() {
    let body = get_attachment_body(&["A1", "A2"]);
    assert!(body.contains("<t:AttachmentId Id=\"A1\"/>"));
    assert!(body.contains("<t:AttachmentId Id=\"A2\"/>"));
    assert!(body.contains("<t:IncludeMimeContent>true</t:IncludeMimeContent>"));
}

#[test]
fn http_500_with_server_busy_body_is_treated_as_fault_and_retried() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let busy_body = format!(
        "<soap:Envelope{NS}><soap:Body><soap:Fault>\
         <faultcode>soap:Server</faultcode><faultstring>busy</faultstring>\
         <detail><ResponseCode xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">ErrorServerBusy</ResponseCode>\
         <t:MessageXml><t:Value Name=\"BackOffMilliseconds\">100</t:Value></t:MessageXml>\
         </detail></soap:Fault></soap:Body></soap:Envelope>"
    );
    let ok_body = envelope(&format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"0\" IncludesLastItemInRange=\"true\"><t:Folders/></m:RootFolder>\
         </m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    ));
    let _m1 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(500)
        .with_header("content-type", TXT_XML)
        .with_body(busy_body)
        .expect(1)
        .create();
    let _m2 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(ok_body)
        .create();

    let c = client(3);
    let body = find_folder_body(
        FolderRef::Distinguished(
            vandelay::exchange_ews::types::DistinguishedFolderId::MsgFolderRoot,
        ),
        Traversal::Deep,
    );
    let r = c.call(&url, "FindFolder", &body).expect("should retry");
    let parsed = parse_find_folder_response(&r.body).unwrap();
    assert!(parsed.folders.is_empty());
}

#[test]
fn invalid_server_version_fault_downgrades_and_succeeds() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let version_fault = format!(
        "<soap:Envelope{NS}><soap:Body><soap:Fault>\
         <faultcode>soap:Server</faultcode>\
         <faultstring>The specified server version is invalid.</faultstring>\
         <detail><ResponseCode xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">ErrorInvalidServerVersion</ResponseCode></detail>\
         </soap:Fault></soap:Body></soap:Envelope>"
    );
    let ok_body = format!(
        "<soap:Envelope{NS}><soap:Header><t:ServerVersionInfo MajorVersion=\"14\" MinorVersion=\"3\"/></soap:Header>\
         <soap:Body><m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"0\" IncludesLastItemInRange=\"true\"><t:Folders/></m:RootFolder>\
         </m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse></soap:Body></soap:Envelope>"
    );

    let m_sp1 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .match_body(Matcher::Regex("Exchange2013_SP1".into()))
        .with_status(500)
        .with_header("content-type", TXT_XML)
        .with_body(&version_fault)
        .expect(1)
        .create();
    let m_2013 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .match_body(Matcher::Regex("Exchange2013\"".into()))
        .with_status(500)
        .with_header("content-type", TXT_XML)
        .with_body(&version_fault)
        .expect(1)
        .create();
    let m_2010 = server
        .mock("POST", "/EWS/Exchange.asmx")
        .match_body(Matcher::Regex("Exchange2010_SP2".into()))
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(&ok_body)
        .expect_at_least(1)
        .create();

    let c = client(0);
    assert_eq!(c.server_version(), ServerVersion::Exchange2013Sp1);
    let body = find_folder_body(
        FolderRef::Distinguished(
            vandelay::exchange_ews::types::DistinguishedFolderId::MsgFolderRoot,
        ),
        Traversal::Deep,
    );
    let r = c
        .call(&url, "FindFolder", &body)
        .expect("call should walk the version ladder down to a version the server accepts");
    let parsed = parse_find_folder_response(&r.body).unwrap();
    assert!(parsed.folders.is_empty());
    assert_eq!(c.server_version(), ServerVersion::Exchange2010Sp2);
    assert_eq!(c.retries_observed(), 0);
    m_sp1.assert();
    m_2013.assert();
    m_2010.assert();
}

#[test]
fn unsupported_version_floor_surfaces_as_soap_fault() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let version_fault = format!(
        "<soap:Envelope{NS}><soap:Body><soap:Fault>\
         <faultcode>soap:Server</faultcode>\
         <faultstring>The specified server version is invalid.</faultstring>\
         <detail><ResponseCode xmlns=\"http://schemas.microsoft.com/exchange/services/2006/types\">ErrorInvalidServerVersion</ResponseCode></detail>\
         </soap:Fault></soap:Body></soap:Envelope>"
    );
    let _m = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(500)
        .with_header("content-type", TXT_XML)
        .with_body(&version_fault)
        .expect_at_least(1)
        .create();

    let c = client(0);
    let body = find_folder_body(
        FolderRef::Distinguished(
            vandelay::exchange_ews::types::DistinguishedFolderId::MsgFolderRoot,
        ),
        Traversal::Deep,
    );
    let err = c.call(&url, "FindFolder", &body).unwrap_err();
    assert!(
        matches!(
            err,
            EwsError::SoapFault {
                code: ResponseCode::InvalidServerVersion,
                ..
            }
        ),
        "got {err:?}"
    );
    assert_eq!(c.server_version(), ServerVersion::Exchange2007);
}

#[test]
fn server_affinity_cookie_is_captured_and_resent() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let ok_body = envelope(&format!(
        "<m:FindFolderResponse{NS}><m:ResponseMessages><m:FindFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:RootFolder TotalItemsInView=\"0\" IncludesLastItemInRange=\"true\"><t:Folders/></m:RootFolder>\
         </m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>"
    ));
    let first = server
        .mock("POST", "/EWS/Exchange.asmx")
        .match_header("x-preferserveraffinity", "True")
        .match_header("x-backendoverridecookie", Matcher::Missing)
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_header(
            "set-cookie",
            "X-BackEndOverrideCookie=AFFIN123; path=/; HttpOnly",
        )
        .with_body(&ok_body)
        .expect(1)
        .create();
    let second = server
        .mock("POST", "/EWS/Exchange.asmx")
        .match_header("x-backendoverridecookie", "AFFIN123")
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(&ok_body)
        .expect(1)
        .create();

    let c = client(0);
    let body = find_folder_body(
        FolderRef::Distinguished(
            vandelay::exchange_ews::types::DistinguishedFolderId::MsgFolderRoot,
        ),
        Traversal::Deep,
    );
    c.call(&url, "FindFolder", &body).expect("first call ok");
    c.call(&url, "FindFolder", &body).expect("second call ok");
    first.assert();
    second.assert();
}

#[test]
fn http_456_surfaces_as_account_locked_auth_error() {
    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let _m = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(456)
        .with_body("Account locked. Unlock at https://unlock.example/")
        .create();
    let c = client(0);
    let body = find_folder_body(
        FolderRef::Distinguished(
            vandelay::exchange_ews::types::DistinguishedFolderId::MsgFolderRoot,
        ),
        Traversal::Deep,
    );
    let err = c.call(&url, "FindFolder", &body).unwrap_err();
    match err {
        EwsError::Auth(m) => assert!(m.contains("locked"), "got {m}"),
        other => panic!("expected Auth error, got {other:?}"),
    }
}

#[test]
fn for_each_fetched_item_streams_every_id_across_windows() {
    use vandelay::logging::Logger;
    use vandelay::sync::import_exchange_ews::items::{ItemRunCtx, for_each_fetched_item};

    let mut server = mockito::Server::new();
    let url = format!("{}/EWS/Exchange.asmx", server.url());
    let one_message = envelope(&format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages><m:GetItemResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode><m:Items><t:Message><t:ItemId Id=\"X\" ChangeKey=\"K\"/></t:Message></m:Items>\
         </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
    ));
    let _m = server
        .mock("POST", "/EWS/Exchange.asmx")
        .with_status(200)
        .with_header("content-type", TXT_XML)
        .with_body(&one_message)
        .expect(5)
        .create();

    let c = client(0);
    let ctx = ItemRunCtx {
        client: &c,
        url: &url,
        source_id: 1,
        batch_size: 1,
        attachment_batch: 1,
        connections: 2,
        use_syncfolderitems: false,
        sync_batch: 512,
        logger: Logger::new(0),
    };
    let ids: Vec<ItemId> = (0..5).map(|i| ItemId::new(format!("I{i}"), "K")).collect();

    let mut delivered = 0usize;
    let failed = for_each_fetched_item(&ctx, ItemShape::Message, &ids, |msg| {
        assert!(msg.success);
        delivered += 1;
        Ok(())
    })
    .expect("streaming fetch should succeed");

    assert_eq!(delivered, 5, "every id must be delivered exactly once");
    assert_eq!(failed, 0);
    _m.assert();
}

#[test]
fn warning_response_class_is_treated_as_success_in_mock() {
    let body = envelope(&format!(
        "<m:GetItemResponse{NS}><m:ResponseMessages>\
         <m:GetItemResponseMessage ResponseClass=\"Warning\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Items><t:Message><t:ItemId Id=\"W1\" ChangeKey=\"K\"/></t:Message></m:Items>\
         </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse>"
    ));
    let r = parse_response_messages(body.as_bytes(), b"GetItemResponseMessage").unwrap();
    assert!(r[0].success);
}

#[test]
fn default_namespace_envelope_parses_via_resolved_names() {
    let body = "<Envelope xmlns=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
                xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
                <Body>\
                <m:FindFolderResponse><m:ResponseMessages>\
                <m:FindFolderResponseMessage ResponseClass=\"Success\">\
                <m:ResponseCode>NoError</m:ResponseCode>\
                <m:RootFolder TotalItemsInView=\"1\" IncludesLastItemInRange=\"true\">\
                <t:Folders><t:Folder><t:FolderId Id=\"D1\"/><t:DisplayName>Inbox</t:DisplayName>\
                </t:Folder></t:Folders></m:RootFolder>\
                </m:FindFolderResponseMessage></m:ResponseMessages></m:FindFolderResponse>\
                </Body></Envelope>";
    let parsed = parse_find_folder_response(body.as_bytes()).unwrap();
    assert_eq!(parsed.folders.len(), 1);
    assert_eq!(parsed.folders[0].folder_id.id, "D1");
}

#[test]
fn get_folder_messages_preserve_position_when_one_errors() {
    let body = envelope(&format!(
        "<m:GetFolderResponse{NS}><m:ResponseMessages>\
         <m:GetFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Folders><t:Folder><t:FolderId Id=\"FA\"/><t:DisplayName>Inbox</t:DisplayName></t:Folder></m:Folders>\
         </m:GetFolderResponseMessage>\
         <m:GetFolderResponseMessage ResponseClass=\"Error\">\
         <m:ResponseCode>ErrorAccessDenied</m:ResponseCode>\
         </m:GetFolderResponseMessage>\
         <m:GetFolderResponseMessage ResponseClass=\"Success\">\
         <m:ResponseCode>NoError</m:ResponseCode>\
         <m:Folders><t:Folder><t:FolderId Id=\"FC\"/><t:DisplayName>Drafts</t:DisplayName></t:Folder></m:Folders>\
         </m:GetFolderResponseMessage>\
         </m:ResponseMessages></m:GetFolderResponse>"
    ));
    let msgs = parse_response_messages(body.as_bytes(), b"GetFolderResponseMessage").unwrap();
    assert_eq!(msgs.len(), 3, "all three messages must be present");
    assert!(msgs[0].success);
    assert!(!msgs[1].success);
    assert!(msgs[2].success);
}
