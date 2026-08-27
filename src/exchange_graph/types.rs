/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxKind {
    Primary,
    Archive,
}

impl MailboxKind {
    pub fn parse(value: &str) -> Result<MailboxKind, Error> {
        match value {
            "primary" => Ok(MailboxKind::Primary),
            "archive" => Ok(MailboxKind::Archive),
            "public-folders" => Err(Error::Usage(
                "Microsoft Graph does not expose public folders. Run `vandelay import exchange-ews --mailbox-kind public-folders` instead.".to_owned(),
            )),
            other => Err(Error::Usage(format!(
                "--mailbox-kind must be primary | archive, got {other:?}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MailboxKind::Primary => "primary",
            MailboxKind::Archive => "archive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventBodyFormat {
    Text,
    Html,
}

impl EventBodyFormat {
    pub fn parse(value: &str) -> Result<EventBodyFormat, Error> {
        match value {
            "text" => Ok(EventBodyFormat::Text),
            "html" => Ok(EventBodyFormat::Html),
            other => Err(Error::Usage(format!(
                "--event-body-format must be text | html, got {other:?}"
            ))),
        }
    }

    pub fn prefer_value(self) -> &'static str {
        match self {
            EventBodyFormat::Text => "outlook.body-content-type=\"text\"",
            EventBodyFormat::Html => "outlook.body-content-type=\"html\"",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surfaces {
    pub mail: bool,
    pub calendar: bool,
    pub contacts: bool,
    pub files: bool,
}

impl Default for Surfaces {
    fn default() -> Self {
        Surfaces::ALL
    }
}

impl Surfaces {
    pub const ALL: Surfaces = Surfaces {
        mail: true,
        calendar: true,
        contacts: true,
        files: true,
    };

    pub const NONE: Surfaces = Surfaces {
        mail: false,
        calendar: false,
        contacts: false,
        files: false,
    };

    pub fn parse_list(list: &str) -> Result<Surfaces, Error> {
        let mut selected = Surfaces::NONE;
        for token in list.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "mail" => selected.mail = true,
                "calendar" => selected.calendar = true,
                "contacts" => selected.contacts = true,
                "files" => selected.files = true,
                _ => {
                    return Err(Error::Usage(format!(
                        "unknown surface: {token} (valid: mail, calendar, contacts, files)"
                    )));
                }
            }
        }
        if selected == Surfaces::NONE {
            return Err(Error::Usage(
                "--objects given but resolved to an empty surface list".to_owned(),
            ));
        }
        Ok(selected)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPrincipal {
    pub id: String,
    pub user_principal_name: String,
}

pub fn synthetic_account_id(directory_id: &str, kind: MailboxKind) -> String {
    match kind {
        MailboxKind::Primary => directory_id.to_owned(),
        MailboxKind::Archive => format!("{directory_id}#archive"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primary_and_archive() {
        assert_eq!(MailboxKind::parse("primary").unwrap(), MailboxKind::Primary);
        assert_eq!(MailboxKind::parse("archive").unwrap(), MailboxKind::Archive);
    }

    #[test]
    fn public_folders_is_redirected_to_ews() {
        let err = MailboxKind::parse("public-folders").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("public folders"));
        assert!(msg.contains("exchange-ews"));
    }

    #[test]
    fn synthetic_id_matches_ews_pattern() {
        assert_eq!(
            synthetic_account_id("u-uuid", MailboxKind::Primary),
            "u-uuid"
        );
        assert_eq!(
            synthetic_account_id("u-uuid", MailboxKind::Archive),
            "u-uuid#archive"
        );
    }

    #[test]
    fn surfaces_default_is_all_three() {
        let all = Surfaces::default();
        assert!(all.mail && all.calendar && all.contacts && all.files);
    }

    #[test]
    fn surfaces_parse_each_name() {
        assert_eq!(
            Surfaces::parse_list("mail").unwrap(),
            Surfaces {
                mail: true,
                calendar: false,
                contacts: false,
                files: false
            }
        );
        assert_eq!(
            Surfaces::parse_list("calendar").unwrap(),
            Surfaces {
                mail: false,
                calendar: true,
                contacts: false,
                files: false
            }
        );
        assert_eq!(
            Surfaces::parse_list("contacts").unwrap(),
            Surfaces {
                mail: false,
                calendar: false,
                contacts: true,
                files: false
            }
        );
    }

    #[test]
    fn surfaces_parse_is_case_insensitive() {
        assert_eq!(
            Surfaces::parse_list("Mail,CALENDAR,Contacts,Files").unwrap(),
            Surfaces::ALL
        );
    }

    #[test]
    fn surfaces_parse_dedups_and_trims() {
        assert_eq!(
            Surfaces::parse_list("  mail , contacts ,mail,, ").unwrap(),
            Surfaces {
                mail: true,
                calendar: false,
                contacts: true,
                files: false
            }
        );
    }

    #[test]
    fn surfaces_parse_rejects_unknown_name() {
        let err = Surfaces::parse_list("mail,contact").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown surface: contact"), "msg was: {msg}");
        assert!(
            msg.contains("valid: mail, calendar, contacts"),
            "msg was: {msg}"
        );
    }

    #[test]
    fn surfaces_parse_rejects_jmap_type_names() {
        assert!(Surfaces::parse_list("mailbox").is_err());
        assert!(Surfaces::parse_list("contactcard").is_err());
    }

    #[test]
    fn surfaces_parse_rejects_empty_list() {
        let err = Surfaces::parse_list("  ,  ").unwrap_err();
        assert!(
            err.to_string().contains("empty surface list"),
            "msg was: {err}"
        );
    }

    #[test]
    fn body_format_prefer_value() {
        assert_eq!(
            EventBodyFormat::Text.prefer_value(),
            "outlook.body-content-type=\"text\""
        );
        assert_eq!(
            EventBodyFormat::Html.prefer_value(),
            "outlook.body-content-type=\"html\""
        );
    }
}
