/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::borrow::Cow;
use std::fmt::{self, Write};
use std::mem;

use serde_json::Value;

use crate::exchange::graph::client::GraphClient;
use crate::exchange::graph::error::GraphError;
use crate::exchange::graph::types::MailboxKind;
use crate::net::encode::SafeSet;

pub const DEFAULT_API_BASE: &str = "https://graph.microsoft.com/v1.0";

pub const PREFER_TIMEZONE_UTC: &str = "outlook.timezone=\"UTC\"";
pub const PREFER_BODY_TEXT: &str = "outlook.body-content-type=\"text\"";
pub const PREFER_BODY_HTML: &str = "outlook.body-content-type=\"html\"";

pub const EXCEPTION_SELECT: &str = "id,type,seriesMasterId,originalStart,iCalUId,subject,body,\
     start,end,isAllDay,isCancelled,isDraft,sensitivity,importance,showAs,categories,locations,\
     organizer,attendees,createdDateTime,lastModifiedDateTime,isReminderOn,\
     reminderMinutesBeforeStart,originalStartTimeZone";

pub const EXCEPTION_WINDOW_MAX_DAYS: i64 = 1825;

pub const DRIVE_ITEM_SELECT: &str =
    "id,name,size,folder,file,package,remoteItem,createdDateTime,lastModifiedDateTime";

const NEXT_LINK: &str = "@odata.nextLink";

pub const MESSAGE_STATE_SELECT: &str = "id,isRead,isDraft,flag,categories";

#[derive(Debug, Clone)]
pub struct Endpoints {
    base: Box<str>,
}

#[derive(Clone, Copy)]
struct Segment<'a>(&'a str);

impl fmt::Display for Segment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.bytes() {
            if SafeSet::Unreserved.contains(byte) {
                f.write_char(char::from(byte))?;
            } else {
                write!(f, "%{byte:02X}")?;
            }
        }
        Ok(())
    }
}

impl Endpoints {
    pub fn for_me(api_base: &str) -> Endpoints {
        Endpoints {
            base: format!("{}/me", api_base.trim_end_matches('/')).into_boxed_str(),
        }
    }

    pub fn for_user(api_base: &str, user_id: &str) -> Endpoints {
        Endpoints {
            base: format!("{}/users/{user_id}", api_base.trim_end_matches('/')).into_boxed_str(),
        }
    }

    pub fn me_or_user(&self) -> &str {
        &self.base
    }

    pub fn mail_folders_root(&self, kind: MailboxKind, top: usize) -> String {
        match kind {
            MailboxKind::Primary => {
                format!(
                    "{}/mailFolders?$top={top}&includeHiddenFolders=true",
                    self.me_or_user()
                )
            }
            MailboxKind::Archive => {
                format!(
                    "{}/mailFolders/archive/childFolders?$top={top}&includeHiddenFolders=true",
                    self.me_or_user()
                )
            }
        }
    }

    pub fn mail_folder_child_folders(&self, folder_id: &str, top: usize) -> String {
        format!(
            "{}/mailFolders/{}/childFolders?$top={top}&includeHiddenFolders=true",
            self.me_or_user(),
            Segment(folder_id)
        )
    }

    pub fn folder_messages_ids(&self, folder_id: &str, top: usize) -> String {
        format!(
            "{}/mailFolders/{}/messages?$top={top}&$select={MESSAGE_STATE_SELECT}",
            self.me_or_user(),
            Segment(folder_id)
        )
    }

    pub fn message_mime(&self, message_id: &str) -> String {
        format!(
            "{}/messages/{}/$value",
            self.me_or_user(),
            Segment(message_id)
        )
    }

    pub fn well_known_folder(&self, well_known: &str) -> String {
        format!(
            "{}/mailFolders/{}?$select=id",
            self.me_or_user(),
            well_known
        )
    }

    pub fn calendars(&self, top: usize) -> String {
        format!("{}/calendars?$top={top}", self.me_or_user())
    }

    pub fn calendar_events_ids(&self, calendar_id: &str, top: usize) -> String {
        format!(
            "{}/calendars/{}/events?$top={top}&$select=id,type,seriesMasterId",
            self.me_or_user(),
            Segment(calendar_id)
        )
    }

    pub fn event(&self, event_id: &str) -> String {
        format!("{}/events/{}", self.me_or_user(), Segment(event_id))
    }

    pub fn calendar_exceptions(
        &self,
        calendar_id: &str,
        window_start: &str,
        window_end: &str,
        top: usize,
    ) -> String {
        format!(
            "{}/calendars/{}/calendarView?startDateTime={window_start}&endDateTime={window_end}\
             &$filter=type%20eq%20%27exception%27&$top={top}&$select={EXCEPTION_SELECT}",
            self.me_or_user(),
            Segment(calendar_id)
        )
    }

    pub fn drive_root(&self) -> String {
        format!("{}/drive/root?$select=id,name", self.me_or_user())
    }

    pub fn drive_children(&self, item_id: &str, top: usize) -> String {
        format!(
            "{}/drive/items/{}/children?$top={top}&$select={DRIVE_ITEM_SELECT}",
            self.me_or_user(),
            Segment(item_id)
        )
    }

    pub fn drive_item_content(&self, item_id: &str) -> String {
        format!(
            "{}/drive/items/{}/content",
            self.me_or_user(),
            Segment(item_id)
        )
    }

    pub fn calendar_occurrences(
        &self,
        calendar_id: &str,
        window_start: &str,
        window_end: &str,
        top: usize,
    ) -> String {
        format!(
            "{}/calendars/{}/calendarView?startDateTime={window_start}&endDateTime={window_end}\
             &$filter=type%20eq%20%27occurrence%27&$top={top}\
             &$select=id,type,seriesMasterId,start,originalStartTimeZone",
            self.me_or_user(),
            Segment(calendar_id)
        )
    }

    pub fn contact_folders(&self, top: usize) -> String {
        format!("{}/contactFolders?$top={top}", self.me_or_user())
    }

    pub fn default_contact_folder(&self) -> String {
        format!("{}/contactFolders/contacts", self.me_or_user())
    }

    pub fn contact_folder(&self, folder_id: &str) -> String {
        format!(
            "{}/contactFolders/{}",
            self.me_or_user(),
            Segment(folder_id)
        )
    }

    pub fn any_contact_parent(&self) -> String {
        format!(
            "{}/contacts?$top=1&$select=id,parentFolderId",
            self.me_or_user()
        )
    }

    pub fn contact_folder_children(&self, folder_id: &str, top: usize) -> String {
        format!(
            "{}/contactFolders/{}/childFolders?$top={top}",
            self.me_or_user(),
            Segment(folder_id)
        )
    }

    pub fn contact_folder_contacts_ids(&self, folder_id: &str, top: usize) -> String {
        format!(
            "{}/contactFolders/{}/contacts?$top={top}&$select=id",
            self.me_or_user(),
            Segment(folder_id)
        )
    }

    pub fn contact(&self, contact_id: &str) -> String {
        format!("{}/contacts/{}", self.me_or_user(), Segment(contact_id))
    }

    pub fn contact_photo(&self, contact_id: &str) -> String {
        format!(
            "{}/contacts/{}/photo/$value",
            self.me_or_user(),
            Segment(contact_id)
        )
    }

    pub fn event_attachments(&self, event_id: &str) -> String {
        format!(
            "{}/events/{}/attachments",
            self.me_or_user(),
            Segment(event_id)
        )
    }

    pub fn mailbox_settings_timezone(&self) -> String {
        format!("{}/mailboxSettings?$select=timeZone", self.me_or_user())
    }

    pub fn me_select_id_upn(&self) -> String {
        format!(
            "{}?$select=id,userPrincipalName,displayName,mail",
            self.me_or_user()
        )
    }
}

impl GraphClient {
    pub fn paged_collect<F>(
        &self,
        initial_url: &str,
        prefer: &[&str],
        mut on_page: F,
    ) -> Result<(), GraphError>
    where
        F: FnMut(Value) -> Result<(), GraphError>,
    {
        let mut url = Cow::Borrowed(initial_url);
        loop {
            let mut body = self.get_json_with_prefer(&url, prefer)?;
            let next = match body.get_mut(NEXT_LINK) {
                Some(Value::String(next)) => Some(mem::take(next)),
                _ => None,
            };
            on_page(body)?;
            match next {
                Some(next) => url = Cow::Owned(next),
                None => return Ok(()),
            }
        }
    }

    pub fn collect_all_ids(
        &self,
        initial_url: &str,
        prefer: &[&str],
    ) -> Result<Vec<String>, GraphError> {
        let mut out = Vec::new();
        self.paged_collect(initial_url, prefer, |mut page| {
            if let Some(Value::Array(items)) = page.get_mut("value") {
                out.extend(
                    items
                        .iter_mut()
                        .filter_map(|item| match item.get_mut("id") {
                            Some(Value::String(id)) => Some(mem::take(id)),
                            _ => None,
                        }),
                );
            }
            Ok(())
        })?;
        Ok(out)
    }

    pub fn collect_all_values(
        &self,
        initial_url: &str,
        prefer: &[&str],
    ) -> Result<Vec<Value>, GraphError> {
        let mut out = Vec::new();
        self.paged_collect(initial_url, prefer, |mut page| {
            if let Some(Value::Array(items)) = page.get_mut("value") {
                out.append(items);
            }
            Ok(())
        })?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_mail_folders_root_uses_me_with_hidden() {
        let e = Endpoints::for_me(DEFAULT_API_BASE);
        let url = e.mail_folders_root(MailboxKind::Primary, 100);
        assert!(url.starts_with("https://graph.microsoft.com/v1.0/me/mailFolders?"));
        assert!(url.contains("includeHiddenFolders=true"));
        assert!(url.contains("$top=100"));
    }

    #[test]
    fn archive_mail_folders_root_uses_archive_child_folders() {
        let e = Endpoints::for_me(DEFAULT_API_BASE);
        let url = e.mail_folders_root(MailboxKind::Archive, 50);
        assert!(
            url.starts_with(
                "https://graph.microsoft.com/v1.0/me/mailFolders/archive/childFolders?"
            )
        );
        assert!(url.contains("$top=50"));
    }

    #[test]
    fn for_user_replaces_me_segment() {
        let e = Endpoints::for_user(DEFAULT_API_BASE, "uid");
        let url = e.mail_folders_root(MailboxKind::Primary, 100);
        assert!(url.contains("/users/uid/mailFolders"));
        assert!(!url.contains("/me/"));
    }

    #[test]
    fn top_threads_through_calendar_and_contact_endpoints() {
        let e = Endpoints::for_me(DEFAULT_API_BASE);
        assert!(e.calendars(75).contains("$top=75"));
        assert!(e.contact_folders(75).contains("$top=75"));
        assert!(e.contact_folder_children("F", 75).contains("$top=75"));
        assert!(e.mail_folder_child_folders("F", 75).contains("$top=75"));
    }

    #[test]
    fn url_escape_encodes_unsafe_bytes_only() {
        assert_eq!(Segment("AAkAAA=").to_string(), "AAkAAA%3D");
        assert_eq!(Segment("plain.id_123-x").to_string(), "plain.id_123-x");
    }

    #[test]
    fn message_mime_endpoint_uses_value() {
        let e = Endpoints::for_me(DEFAULT_API_BASE);
        let url = e.message_mime("AAkA==");
        assert!(url.ends_with("/messages/AAkA%3D%3D/$value"));
    }

    #[test]
    fn well_known_folder_uses_short_name() {
        let e = Endpoints::for_me(DEFAULT_API_BASE);
        let url = e.well_known_folder("inbox");
        assert!(url.ends_with("/mailFolders/inbox?$select=id"));
    }

    #[test]
    fn event_ids_endpoint_requests_routing_fields() {
        let e = Endpoints::for_me(DEFAULT_API_BASE);
        let url = e.calendar_events_ids("CAL", 100);
        assert!(url.contains("$select=id,type,seriesMasterId"));
        assert!(url.contains("$top=100"));
    }
}
