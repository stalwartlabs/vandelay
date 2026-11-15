/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

const ARCHIVE_SUFFIX: &str = "#archive";
const PUBLIC_FOLDERS_LOCAL: &str = "__public_folders__@";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxKind {
    Primary,
    Archive,
    PublicFolders,
}

impl MailboxKind {
    pub fn parse(value: &str) -> Option<MailboxKind> {
        hashify::map!(
            value.as_bytes(),
            MailboxKind,
            "primary" => MailboxKind::Primary,
            "archive" => MailboxKind::Archive,
            "public-folders" => MailboxKind::PublicFolders,
        )
        .copied()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MailboxKind::Primary => "primary",
            MailboxKind::Archive => "archive",
            MailboxKind::PublicFolders => "public-folders",
        }
    }

    pub fn account_id(self, owner: &str) -> SyntheticAccountId<'_> {
        SyntheticAccountId { owner, kind: self }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyntheticAccountId<'a> {
    owner: &'a str,
    kind: MailboxKind,
}

impl fmt::Display for SyntheticAccountId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            MailboxKind::Primary => f.write_str(self.owner),
            MailboxKind::Archive => {
                f.write_str(self.owner)?;
                f.write_str(ARCHIVE_SUFFIX)
            }
            MailboxKind::PublicFolders => {
                f.write_str(PUBLIC_FOLDERS_LOCAL)?;
                f.write_str(self.owner.split('@').nth(1).unwrap_or_default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_uses_the_owner_verbatim() {
        assert_eq!(
            MailboxKind::Primary
                .account_id("alice@contoso.com")
                .to_string(),
            "alice@contoso.com"
        );
    }

    #[test]
    fn archive_appends_the_suffix() {
        assert_eq!(
            MailboxKind::Archive.account_id("u-uuid").to_string(),
            "u-uuid#archive"
        );
    }

    #[test]
    fn public_folders_use_the_tenant_domain() {
        assert_eq!(
            MailboxKind::PublicFolders
                .account_id("alice@contoso.com")
                .to_string(),
            "__public_folders__@contoso.com"
        );
        assert_eq!(
            MailboxKind::PublicFolders.account_id("alice").to_string(),
            "__public_folders__@"
        );
    }

    #[test]
    fn kinds_parse_exactly() {
        for kind in [
            MailboxKind::Primary,
            MailboxKind::Archive,
            MailboxKind::PublicFolders,
        ] {
            assert_eq!(MailboxKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(MailboxKind::parse("Primary"), None);
    }
}
