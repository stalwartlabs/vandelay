/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::Value;

use crate::exchange_graph::client::GraphClient;
use crate::exchange_graph::error::GraphError;
use crate::exchange_graph::types::MailboxKind;

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

pub const MESSAGE_STATE_SELECT: &str = "id,isRead,isDraft,isReadReceiptRequested,flag,categories";

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub api_base: String,
    pub user_path: String,
}

impl Endpoints {
    pub fn for_me(api_base: &str) -> Endpoints {
        Endpoints {
            api_base: api_base.trim_end_matches('/').to_owned(),
            user_path: "/me".to_owned(),
        }
    }

    pub fn for_user(api_base: &str, user_id: &str) -> Endpoints {
        Endpoints {
            api_base: api_base.trim_end_matches('/').to_owned(),
            user_path: format!("/users/{user_id}"),
        }
    }

    pub fn me_or_user(&self) -> String {
        format!("{}{}", self.api_base, self.user_path)
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
            url_escape(folder_id)
        )
    }

    pub fn folder_messages_ids(&self, folder_id: &str, top: usize) -> String {
        format!(
            "{}/mailFolders/{}/messages?$top={top}&$select={MESSAGE_STATE_SELECT}",
            self.me_or_user(),
            url_escape(folder_id)
        )
    }

    pub fn message_mime(&self, message_id: &str) -> String {
        format!(
            "{}/messages/{}/$value",
            self.me_or_user(),
            url_escape(message_id)
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
            url_escape(calendar_id)
        )
    }

    pub fn event(&self, event_id: &str) -> String {
        format!("{}/events/{}", self.me_or_user(), url_escape(event_id))
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
            url_escape(calendar_id)
        )
    }

    pub fn drive_root(&self) -> String {
        format!("{}/drive/root?$select=id,name", self.me_or_user())
    }

    pub fn drive_children(&self, item_id: &str, top: usize) -> String {
        format!(
            "{}/drive/items/{}/children?$top={top}&$select={DRIVE_ITEM_SELECT}",
            self.me_or_user(),
            url_escape(item_id)
        )
    }

    pub fn drive_item_content(&self, item_id: &str) -> String {
        format!(
            "{}/drive/items/{}/content",
            self.me_or_user(),
            url_escape(item_id)
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
            url_escape(folder_id)
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
            url_escape(folder_id)
        )
    }

    pub fn contact_folder_contacts_ids(&self, folder_id: &str, top: usize) -> String {
        format!(
            "{}/contactFolders/{}/contacts?$top={top}&$select=id",
            self.me_or_user(),
            url_escape(folder_id)
        )
    }

    pub fn contact(&self, contact_id: &str) -> String {
        format!("{}/contacts/{}", self.me_or_user(), url_escape(contact_id))
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

fn url_escape(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

pub fn paged_collect<F>(
    client: &GraphClient,
    initial_url: &str,
    prefer: &[&str],
    mut on_page: F,
) -> Result<(), GraphError>
where
    F: FnMut(&Value) -> Result<(), GraphError>,
{
    let mut url = initial_url.to_owned();
    loop {
        let body = client.get_json_with_prefer(&url, prefer)?;
        on_page(&body)?;
        match body.get("@odata.nextLink").and_then(Value::as_str) {
            Some(next) => url = next.to_owned(),
            None => return Ok(()),
        }
    }
}

pub fn collect_all_ids(
    client: &GraphClient,
    initial_url: &str,
    prefer: &[&str],
) -> Result<Vec<String>, GraphError> {
    let mut out = Vec::new();
    paged_collect(client, initial_url, prefer, |page| {
        if let Some(arr) = page.get("value").and_then(Value::as_array) {
            for item in arr {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    out.push(id.to_owned());
                }
            }
        }
        Ok(())
    })?;
    Ok(out)
}

pub fn collect_all_values(
    client: &GraphClient,
    initial_url: &str,
    prefer: &[&str],
) -> Result<Vec<Value>, GraphError> {
    let mut out = Vec::new();
    paged_collect(client, initial_url, prefer, |page| {
        if let Some(arr) = page.get("value").and_then(Value::as_array) {
            for item in arr {
                out.push(item.clone());
            }
        }
        Ok(())
    })?;
    Ok(out)
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
        assert_eq!(url_escape("AAkAAA="), "AAkAAA%3D");
        assert_eq!(url_escape("plain.id_123-x"), "plain.id_123-x");
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
