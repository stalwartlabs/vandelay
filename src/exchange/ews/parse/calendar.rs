/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::mem;

use quick_xml::events::Event;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::attachment::AttachmentElement;
use crate::exchange::ews::parse::recurrence::{
    PendingRecurrence, RecurrenceElement, RecurrenceText,
};
use crate::exchange::ews::parse::{
    Attrs, ItemDepth, ItemElement, Ns, RawAttachmentRef, RawRecurrence, TextBuf, Trim, XmlBool,
};
use crate::exchange::ews::types::{CalendarItemType, ItemId};

#[derive(Debug, Clone, Default)]
pub struct CalendarItemRaw {
    pub element: String,
    pub id: ItemId,
    pub parent_folder_id: Option<String>,
    pub uid: Option<String>,
    pub subject: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub original_start: Option<String>,
    pub is_all_day_event: Option<bool>,
    pub legacy_free_busy_status: Option<String>,
    pub my_response_type: Option<String>,
    pub sensitivity: Option<String>,
    pub location: Option<String>,
    pub calendar_item_type: Option<CalendarItemType>,
    pub recurrence_id: Option<String>,
    pub start_tz: Option<String>,
    pub end_tz: Option<String>,
    pub recurrence: Option<RawRecurrence>,
    pub modified_occurrences: Vec<RawOccurrence>,
    pub deleted_occurrences: Vec<RawOccurrence>,
    pub organizer_smtp: Option<String>,
    pub organizer_routing_type: Option<String>,
    pub organizer_name: Option<String>,
    pub required_attendees: Vec<RawAttendee>,
    pub optional_attendees: Vec<RawAttendee>,
    pub resources: Vec<RawAttendee>,
    pub reminder_is_set: Option<bool>,
    pub reminder_minutes_before_start: Option<i64>,
    pub is_online_meeting: Option<bool>,
    pub join_online_meeting_url: Option<String>,
    pub net_show_url: Option<String>,
    pub meeting_workspace_url: Option<String>,
    pub categories: Vec<String>,
    pub created: Option<String>,
    pub last_modified: Option<String>,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub attachments: Vec<RawAttachmentRef>,
}

#[derive(Debug, Clone, Default)]
pub struct RawOccurrence {
    pub item_id: ItemId,
    pub start: Option<String>,
    pub end: Option<String>,
    pub original_start: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RawAttendee {
    pub email: Option<String>,
    pub routing_type: Option<String>,
    pub name: Option<String>,
    pub response_type: Option<String>,
}

#[derive(Clone, Copy)]
enum CalendarField {
    ItemId,
    ParentFolderId,
    Uid,
    Subject,
    Start,
    End,
    OriginalStart,
    IsAllDayEvent,
    LegacyFreeBusyStatus,
    MyResponseType,
    Sensitivity,
    Location,
    CalendarItemType,
    RecurrenceId,
    StartTimeZone,
    EndTimeZone,
    Recurrence,
    ModifiedOccurrences,
    DeletedOccurrences,
    Occurrence,
    DeletedOccurrence,
    Organizer,
    Attendees(AttendeeKind),
    ReminderIsSet,
    ReminderMinutesBeforeStart,
    IsOnlineMeeting,
    JoinOnlineMeetingUrl,
    NetShowUrl,
    MeetingWorkspaceUrl,
    Attendee,
    Mailbox,
    Name,
    EmailAddress,
    RoutingType,
    ResponseType,
    Categories,
    String,
    DateTimeCreated,
    LastModifiedTime,
    Body,
    Attachment(AttachmentElement),
}

impl CalendarField {
    fn parse(local: &str) -> Option<CalendarField> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            CalendarField,
            "ItemId" => CalendarField::ItemId,
            "ParentFolderId" => CalendarField::ParentFolderId,
            "UID" => CalendarField::Uid,
            "Subject" => CalendarField::Subject,
            "Start" => CalendarField::Start,
            "End" => CalendarField::End,
            "OriginalStart" => CalendarField::OriginalStart,
            "IsAllDayEvent" => CalendarField::IsAllDayEvent,
            "LegacyFreeBusyStatus" => CalendarField::LegacyFreeBusyStatus,
            "MyResponseType" => CalendarField::MyResponseType,
            "Sensitivity" => CalendarField::Sensitivity,
            "Location" => CalendarField::Location,
            "CalendarItemType" => CalendarField::CalendarItemType,
            "RecurrenceId" => CalendarField::RecurrenceId,
            "StartTimeZone" => CalendarField::StartTimeZone,
            "EndTimeZone" => CalendarField::EndTimeZone,
            "Recurrence" => CalendarField::Recurrence,
            "ModifiedOccurrences" => CalendarField::ModifiedOccurrences,
            "DeletedOccurrences" => CalendarField::DeletedOccurrences,
            "Occurrence" => CalendarField::Occurrence,
            "DeletedOccurrence" => CalendarField::DeletedOccurrence,
            "Organizer" => CalendarField::Organizer,
            "RequiredAttendees" => CalendarField::Attendees(AttendeeKind::Required),
            "OptionalAttendees" => CalendarField::Attendees(AttendeeKind::Optional),
            "Resources" => CalendarField::Attendees(AttendeeKind::Resource),
            "ReminderIsSet" => CalendarField::ReminderIsSet,
            "ReminderMinutesBeforeStart" => CalendarField::ReminderMinutesBeforeStart,
            "IsOnlineMeeting" => CalendarField::IsOnlineMeeting,
            "JoinOnlineMeetingUrl" => CalendarField::JoinOnlineMeetingUrl,
            "NetShowUrl" => CalendarField::NetShowUrl,
            "MeetingWorkspaceUrl" => CalendarField::MeetingWorkspaceUrl,
            "Attendee" => CalendarField::Attendee,
            "Mailbox" => CalendarField::Mailbox,
            "Name" => CalendarField::Name,
            "EmailAddress" => CalendarField::EmailAddress,
            "RoutingType" => CalendarField::RoutingType,
            "ResponseType" => CalendarField::ResponseType,
            "Categories" => CalendarField::Categories,
            "String" => CalendarField::String,
            "DateTimeCreated" => CalendarField::DateTimeCreated,
            "LastModifiedTime" => CalendarField::LastModifiedTime,
            "Body" => CalendarField::Body,
            "FileAttachment" => CalendarField::Attachment(AttachmentElement::File),
            "ItemAttachment" => CalendarField::Attachment(AttachmentElement::Item),
        )
        .copied()
    }
}

#[derive(Clone, Copy)]
enum AttendeeKind {
    Required,
    Optional,
    Resource,
}

#[derive(Clone, Copy)]
enum Party {
    Organizer,
    Attendee,
}

#[derive(Clone, Copy)]
enum CalendarText {
    Uid,
    Subject,
    Start,
    End,
    OriginalStart,
    OccurrenceStart,
    OccurrenceEnd,
    OccurrenceOriginalStart,
    IsAllDay,
    FreeBusy,
    MyResponse,
    Sensitivity,
    Location,
    CalendarItemType,
    RecurrenceId,
    Name(Party),
    Email(Party),
    Routing(Party),
    AttendeeResponse,
    ReminderIsSet,
    ReminderMinutes,
    IsOnlineMeeting,
    JoinUrl,
    NetShowUrl,
    WorkspaceUrl,
    Category,
    Created,
    LastModified,
    BodyText,
    BodyHtml,
}

#[derive(Default)]
struct CalendarState {
    item: CalendarItemRaw,
    text_target: Option<CalendarText>,
    category_collecting: bool,
    attendee_kind: Option<AttendeeKind>,
    current_attendee: Option<RawAttendee>,
    in_mailbox: bool,
    in_organizer: bool,
    occurrence_stack: Vec<RawOccurrence>,
    deleted_stack: Vec<RawOccurrence>,
    in_modified: bool,
    in_deleted: bool,
}

impl CalendarState {
    fn party(&self) -> Party {
        if self.in_organizer {
            Party::Organizer
        } else {
            Party::Attendee
        }
    }

    fn in_occurrences(&self) -> bool {
        self.in_modified || self.in_deleted
    }

    fn close_text(&mut self, target: CalendarText, text: String) {
        let item = &mut self.item;
        match target {
            CalendarText::Uid => item.uid = Some(text),
            CalendarText::Subject => item.subject = Some(text),
            CalendarText::Start => item.start = Some(text),
            CalendarText::End => item.end = Some(text),
            CalendarText::OriginalStart => item.original_start = Some(text),
            CalendarText::IsAllDay => item.is_all_day_event = Some(XmlBool::parse(&text)),
            CalendarText::FreeBusy => item.legacy_free_busy_status = Some(text),
            CalendarText::MyResponse => item.my_response_type = Some(text.trim().to_owned()),
            CalendarText::Sensitivity => item.sensitivity = Some(text.trim().to_owned()),
            CalendarText::Location => item.location = Some(text),
            CalendarText::CalendarItemType => {
                item.calendar_item_type = CalendarItemType::parse(text.trim());
            }
            CalendarText::RecurrenceId => item.recurrence_id = Some(text),
            CalendarText::Name(Party::Organizer) => item.organizer_name = Some(text),
            CalendarText::Email(Party::Organizer) => item.organizer_smtp = Some(text),
            CalendarText::Routing(Party::Organizer) => item.organizer_routing_type = Some(text),
            CalendarText::Name(Party::Attendee) => {
                if let Some(att) = self.current_attendee.as_mut() {
                    att.name = Some(text);
                }
            }
            CalendarText::Email(Party::Attendee) => {
                if let Some(att) = self.current_attendee.as_mut() {
                    att.email = Some(text);
                }
            }
            CalendarText::Routing(Party::Attendee) => {
                if let Some(att) = self.current_attendee.as_mut() {
                    att.routing_type = Some(text);
                }
            }
            CalendarText::AttendeeResponse => {
                if let Some(att) = self.current_attendee.as_mut() {
                    att.response_type = Some(text);
                }
            }
            CalendarText::ReminderIsSet => item.reminder_is_set = Some(XmlBool::parse(&text)),
            CalendarText::ReminderMinutes => {
                item.reminder_minutes_before_start = text.trim().parse().ok();
            }
            CalendarText::IsOnlineMeeting => item.is_online_meeting = Some(XmlBool::parse(&text)),
            CalendarText::JoinUrl => item.join_online_meeting_url = Some(text),
            CalendarText::NetShowUrl => item.net_show_url = Some(text),
            CalendarText::WorkspaceUrl => item.meeting_workspace_url = Some(text),
            CalendarText::Category => item.categories.push(text),
            CalendarText::Created => item.created = Some(text),
            CalendarText::LastModified => item.last_modified = Some(text),
            CalendarText::BodyText => item.body_text = Some(text),
            CalendarText::BodyHtml => item.body_html = Some(text),
            CalendarText::OccurrenceStart => {
                match (
                    self.occurrence_stack.last_mut(),
                    self.deleted_stack.last_mut(),
                ) {
                    (Some(occ), Some(deleted)) => {
                        occ.start = Some(text.clone());
                        deleted.start = Some(text);
                    }
                    (Some(occ), None) => occ.start = Some(text),
                    (None, Some(deleted)) => deleted.start = Some(text),
                    (None, None) => {}
                }
            }
            CalendarText::OccurrenceEnd => {
                if let Some(occ) = self.occurrence_stack.last_mut() {
                    occ.end = Some(text);
                }
            }
            CalendarText::OccurrenceOriginalStart => {
                if let Some(occ) = self.occurrence_stack.last_mut() {
                    occ.original_start = Some(text);
                }
            }
        }
    }

    fn close_element(&mut self, field: CalendarField) {
        match field {
            CalendarField::ModifiedOccurrences => self.in_modified = false,
            CalendarField::DeletedOccurrences => self.in_deleted = false,
            CalendarField::Occurrence => {
                if let Some(occ) = self.occurrence_stack.pop() {
                    self.item.modified_occurrences.push(occ);
                }
            }
            CalendarField::DeletedOccurrence => {
                if let Some(occ) = self.deleted_stack.pop() {
                    self.item.deleted_occurrences.push(occ);
                }
            }
            CalendarField::Attendee => {
                if let Some(att) = self.current_attendee.take() {
                    match self.attendee_kind {
                        Some(AttendeeKind::Required) => self.item.required_attendees.push(att),
                        Some(AttendeeKind::Optional) => self.item.optional_attendees.push(att),
                        Some(AttendeeKind::Resource) => self.item.resources.push(att),
                        None => {}
                    }
                }
            }
            CalendarField::Attendees(_) => self.attendee_kind = None,
            CalendarField::Organizer => self.in_organizer = false,
            CalendarField::Mailbox => self.in_mailbox = false,
            CalendarField::Categories => self.category_collecting = false,
            _ => {}
        }
    }
}

impl CalendarItemRaw {
    pub fn parse(inner_xml: &str) -> Result<CalendarItemRaw, EwsError> {
        let mut xml = Trim::Keep.reader(inner_xml.as_bytes());
        let mut buf = Vec::new();
        let mut st = CalendarState::default();
        let mut recurrence_depth: usize = 0;
        let mut recurrence_text: Option<RecurrenceText> = None;
        let mut pending = PendingRecurrence::default();
        let mut depth = ItemDepth::default();
        let mut cur = TextBuf::default();

        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let local = e.local_name().into_inner();
                    let is_empty = matches!(ev, Event::Empty(_));
                    let level = depth.open(is_empty);
                    if st.item.element.is_empty() && ItemElement::is(local) {
                        local.clone_into(&mut st.item.element);
                        depth.enter_item(level);
                        continue;
                    }
                    if Ns::of(&ns) != Ns::Types {
                        continue;
                    }
                    if recurrence_depth > 0 {
                        match RecurrenceElement::parse(local) {
                            Some(RecurrenceElement::Text(target)) => {
                                recurrence_text = Some(target);
                            }
                            Some(RecurrenceElement::Pattern(kind)) => pending.pattern = Some(kind),
                            Some(RecurrenceElement::Range(kind)) => pending.range = Some(kind),
                            None => recurrence_text = None,
                        }
                        if is_empty {
                            recurrence_text = None;
                        } else {
                            recurrence_depth += 1;
                        }
                        continue;
                    }
                    cur.0.clear();
                    let field = CalendarField::parse(local);
                    let text = match field {
                        Some(CalendarField::ItemId) => {
                            let target = if st.in_modified {
                                st.occurrence_stack.last_mut().map(|occ| &mut occ.item_id)
                            } else if st.in_deleted {
                                st.deleted_stack.last_mut().map(|occ| &mut occ.item_id)
                            } else if depth.is_item_child(level) {
                                Some(&mut st.item.id)
                            } else {
                                None
                            };
                            if let Some(id) = target {
                                Attrs(e).ids(&mut id.id, &mut id.change_key);
                            }
                            st.text_target
                        }
                        Some(CalendarField::ParentFolderId) => {
                            if let Some(pid) = Attrs(e).parent_id() {
                                st.item.parent_folder_id = Some(pid);
                            }
                            st.text_target
                        }
                        Some(CalendarField::Uid) => Some(CalendarText::Uid),
                        Some(CalendarField::Subject) => Some(CalendarText::Subject),
                        Some(CalendarField::Start) if st.in_occurrences() => {
                            Some(CalendarText::OccurrenceStart)
                        }
                        Some(CalendarField::Start) => Some(CalendarText::Start),
                        Some(CalendarField::End) if st.in_occurrences() => {
                            Some(CalendarText::OccurrenceEnd)
                        }
                        Some(CalendarField::End) => Some(CalendarText::End),
                        Some(CalendarField::OriginalStart) if st.in_occurrences() => {
                            Some(CalendarText::OccurrenceOriginalStart)
                        }
                        Some(CalendarField::OriginalStart) => Some(CalendarText::OriginalStart),
                        Some(CalendarField::IsAllDayEvent) => Some(CalendarText::IsAllDay),
                        Some(CalendarField::LegacyFreeBusyStatus) => Some(CalendarText::FreeBusy),
                        Some(CalendarField::MyResponseType) => Some(CalendarText::MyResponse),
                        Some(CalendarField::Sensitivity) => Some(CalendarText::Sensitivity),
                        Some(CalendarField::Location) => Some(CalendarText::Location),
                        Some(CalendarField::CalendarItemType) => {
                            Some(CalendarText::CalendarItemType)
                        }
                        Some(CalendarField::RecurrenceId) => Some(CalendarText::RecurrenceId),
                        Some(CalendarField::StartTimeZone) => {
                            if let Some(v) = Attrs(e).value("Id") {
                                st.item.start_tz = Some(v);
                            }
                            st.text_target
                        }
                        Some(CalendarField::EndTimeZone) => {
                            if let Some(v) = Attrs(e).value("Id") {
                                st.item.end_tz = Some(v);
                            }
                            st.text_target
                        }
                        Some(CalendarField::Recurrence) => {
                            recurrence_depth = 1;
                            st.text_target
                        }
                        Some(CalendarField::ModifiedOccurrences) => {
                            st.in_modified = true;
                            st.text_target
                        }
                        Some(CalendarField::DeletedOccurrences) => {
                            st.in_deleted = true;
                            st.text_target
                        }
                        Some(CalendarField::Occurrence) => {
                            let mut occ = RawOccurrence::default();
                            Attrs(e).ids(&mut occ.item_id.id, &mut occ.item_id.change_key);
                            st.occurrence_stack.push(occ);
                            st.text_target
                        }
                        Some(CalendarField::DeletedOccurrence) => {
                            st.deleted_stack.push(RawOccurrence::default());
                            st.text_target
                        }
                        Some(CalendarField::Organizer) => {
                            st.in_organizer = true;
                            st.text_target
                        }
                        Some(CalendarField::Attendees(kind)) => {
                            st.attendee_kind = Some(kind);
                            st.text_target
                        }
                        Some(CalendarField::ReminderIsSet) => Some(CalendarText::ReminderIsSet),
                        Some(CalendarField::ReminderMinutesBeforeStart) => {
                            Some(CalendarText::ReminderMinutes)
                        }
                        Some(CalendarField::IsOnlineMeeting) => Some(CalendarText::IsOnlineMeeting),
                        Some(CalendarField::JoinOnlineMeetingUrl) => Some(CalendarText::JoinUrl),
                        Some(CalendarField::NetShowUrl) => Some(CalendarText::NetShowUrl),
                        Some(CalendarField::MeetingWorkspaceUrl) => {
                            Some(CalendarText::WorkspaceUrl)
                        }
                        Some(CalendarField::Attendee) if st.attendee_kind.is_some() => {
                            st.current_attendee = Some(RawAttendee::default());
                            st.text_target
                        }
                        Some(CalendarField::Mailbox) => {
                            st.in_mailbox = true;
                            st.text_target
                        }
                        Some(CalendarField::Name) if st.in_mailbox => {
                            Some(CalendarText::Name(st.party()))
                        }
                        Some(CalendarField::EmailAddress) if st.in_mailbox => {
                            Some(CalendarText::Email(st.party()))
                        }
                        Some(CalendarField::RoutingType) if st.in_mailbox => {
                            Some(CalendarText::Routing(st.party()))
                        }
                        Some(CalendarField::ResponseType) if st.current_attendee.is_some() => {
                            Some(CalendarText::AttendeeResponse)
                        }
                        Some(CalendarField::Categories) => {
                            st.category_collecting = true;
                            st.text_target
                        }
                        Some(CalendarField::String) if st.category_collecting => {
                            Some(CalendarText::Category)
                        }
                        Some(CalendarField::DateTimeCreated) => Some(CalendarText::Created),
                        Some(CalendarField::LastModifiedTime) => Some(CalendarText::LastModified),
                        Some(CalendarField::Body) => {
                            let html = Attrs(e)
                                .value("BodyType")
                                .is_some_and(|t| t.eq_ignore_ascii_case("HTML"));
                            Some(if html {
                                CalendarText::BodyHtml
                            } else {
                                CalendarText::BodyText
                            })
                        }
                        Some(CalendarField::Attachment(element)) => {
                            let att = RawAttachmentRef::read(&mut xml, element)?;
                            st.item.attachments.push(att);
                            st.text_target
                        }
                        _ => None,
                    };
                    st.text_target = if is_empty { None } else { text };
                }
                Event::End(e) => {
                    depth.close();
                    if recurrence_depth > 0 {
                        recurrence_depth -= 1;
                        if recurrence_depth == 0 {
                            st.item.recurrence = Some(mem::take(&mut pending).finish());
                        }
                        recurrence_text = None;
                        continue;
                    }
                    if let Some(target) = st.text_target.take() {
                        st.close_text(target, cur.take());
                    }
                    cur.0.clear();
                    if let Some(field) = CalendarField::parse(e.local_name().into_inner()) {
                        st.close_element(field);
                    }
                }
                Event::Text(ref t) => {
                    if recurrence_depth > 0 {
                        if let Some(target) = recurrence_text {
                            pending.text(target, t);
                        }
                    } else {
                        cur.0.push_str(t);
                    }
                }
                Event::CData(ref c) => {
                    if recurrence_depth == 0 {
                        cur.0.push_str(c);
                    }
                }
                Event::GeneralRef(ref g) => {
                    if recurrence_depth == 0 {
                        cur.push_ref(g);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(st.item)
    }
}
