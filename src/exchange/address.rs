/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

const ATEXT_SYMBOLS: &[u8] = b"!#$%&'*+-/=?^_`{|}~";
const ROUTING_PREFIXES: [&str; 2] = ["mailto:", "smtp:"];
const MAX_LOCAL_PART: usize = 64;
const MAX_DOMAIN: usize = 255;
const MAX_LABEL: usize = 63;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpAddress(String);

impl SmtpAddress {
    pub fn parse(raw: &str) -> Option<SmtpAddress> {
        let candidate = SmtpAddress::normalise(raw)?;
        SmtpAddress::is_addr_spec(candidate).then(|| SmtpAddress(candidate.to_owned()))
    }

    pub fn extract(raw: &str) -> Option<SmtpAddress> {
        let candidate = SmtpAddress::normalise(raw)?;
        let (_, domain) = candidate.rsplit_once('@')?;
        (SmtpAddress::is_addr_spec(candidate)
            && psl::suffix(domain.as_bytes()).is_some_and(|s| s.is_known()))
        .then(|| SmtpAddress(candidate.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    fn normalise(raw: &str) -> Option<&str> {
        let mut value = raw.trim();
        if let Some((_, after)) = value.rsplit_once('<')
            && let Some((inner, _)) = after.rsplit_once('>')
        {
            value = inner.trim();
        }
        for prefix in ROUTING_PREFIXES {
            if value.len() > prefix.len()
                && let Some(head) = value.get(..prefix.len())
                && let Some(rest) = value.get(prefix.len()..)
                && head.eq_ignore_ascii_case(prefix)
            {
                value = rest.trim();
            }
        }
        (!value.is_empty()).then_some(value)
    }

    fn is_addr_spec(value: &str) -> bool {
        if !value.is_ascii() || value.starts_with('/') {
            return false;
        }
        let Some((local, domain)) = value.split_once('@') else {
            return false;
        };
        !domain.contains('@') && SmtpAddress::is_local_part(local) && SmtpAddress::is_domain(domain)
    }

    fn is_local_part(local: &str) -> bool {
        if local.is_empty()
            || local.len() > MAX_LOCAL_PART
            || local.starts_with('.')
            || local.ends_with('.')
            || local.contains("..")
        {
            return false;
        }
        local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || ATEXT_SYMBOLS.contains(&b))
    }

    fn is_domain(domain: &str) -> bool {
        if domain.is_empty() || domain.len() > MAX_DOMAIN {
            return false;
        }
        domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= MAX_LABEL
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXCHANGE_DN: &str = "/o=ExchangeLabs/ou=Exchange Administrative Group \
         (FYDIBOHF23SPDLT)/cn=Recipients/cn=bdc77b18152647a29d28ce1188376dc9-kristina";

    #[test]
    fn plain_addresses_are_accepted() {
        assert_eq!(
            SmtpAddress::parse("user@example.com")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("user@example.com")
        );
        assert_eq!(
            SmtpAddress::parse("  First.Last@sub.example.co.uk  ")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("First.Last@sub.example.co.uk")
        );
    }

    #[test]
    fn internal_only_tlds_are_kept_when_the_server_says_smtp() {
        assert_eq!(
            SmtpAddress::parse("user@contoso.local")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("user@contoso.local")
        );
        assert!(
            SmtpAddress::extract("user@contoso.local").is_none(),
            "an unknown suffix is not enough to override the routing type"
        );
    }

    #[test]
    fn routing_prefixes_and_display_names_are_stripped() {
        assert_eq!(
            SmtpAddress::parse("SMTP:user@example.com")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("user@example.com")
        );
        assert_eq!(
            SmtpAddress::parse("mailto:user@example.com")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("user@example.com")
        );
        assert_eq!(
            SmtpAddress::parse("Jane Doe <jane@example.com>")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("jane@example.com")
        );
        assert_eq!(
            SmtpAddress::extract("SMTP:jane@example.com")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("jane@example.com")
        );
    }

    #[test]
    fn entra_guest_upns_are_accepted() {
        let guest = "bob_contoso.com#EXT#@fabrikam.onmicrosoft.com";
        assert_eq!(
            SmtpAddress::parse(guest).as_ref().map(SmtpAddress::as_str),
            Some(guest)
        );
        assert_eq!(
            SmtpAddress::extract(guest)
                .as_ref()
                .map(SmtpAddress::as_str),
            Some(guest)
        );
    }

    #[test]
    fn legacy_exchange_distinguished_names_are_rejected() {
        assert!(SmtpAddress::parse(EXCHANGE_DN).is_none());
        assert!(SmtpAddress::extract(EXCHANGE_DN).is_none());
        assert!(
            SmtpAddress::parse(
                "/O=HOSTING/OU=EXCHANGE ADMINISTRATIVE GROUP (FYDIBOHF23SPDLT)/CN=RECIPIENTS/CN=abc"
            )
            .is_none()
        );
    }

    #[test]
    fn malformed_values_are_rejected() {
        for bad in [
            "",
            "   ",
            "no-at-sign",
            "@example.com",
            "user@",
            "user@@example.com",
            "two words@example.com",
            "user@-example.com",
            "user@example-.com",
            ".user@example.com",
            "user.@example.com",
            "us..er@example.com",
            "üser@example.com",
        ] {
            assert!(SmtpAddress::parse(bad).is_none(), "must reject {bad:?}");
            assert!(SmtpAddress::extract(bad).is_none(), "must reject {bad:?}");
        }
    }

    #[test]
    fn over_long_parts_are_rejected() {
        let long_local = format!("{}@example.com", "a".repeat(65));
        assert!(SmtpAddress::parse(&long_local).is_none());
        let long_label = format!("user@{}.com", "a".repeat(64));
        assert!(SmtpAddress::parse(&long_label).is_none());
    }

    #[test]
    fn dotless_domains_are_kept_when_the_server_says_smtp() {
        assert_eq!(
            SmtpAddress::parse("postmaster@localhost")
                .as_ref()
                .map(SmtpAddress::as_str),
            Some("postmaster@localhost"),
            "RFC 5321 allows a bare domain and old on-prem setups use them"
        );
        assert!(SmtpAddress::extract("postmaster@localhost").is_none());
    }

    #[test]
    fn public_suffix_is_required_only_by_the_strict_extractor() {
        assert!(SmtpAddress::extract("user@example.com").is_some());
        assert!(SmtpAddress::extract("user@example.co.uk").is_some());
        assert!(SmtpAddress::extract("user@example.invalidtld").is_none());
        assert!(SmtpAddress::parse("user@example.invalidtld").is_some());
    }

    #[test]
    fn multibyte_text_at_the_prefix_boundary_does_not_panic() {
        // Regression: the routing-prefix check sliced a str at a byte offset.
        assert!(SmtpAddress::parse("ab\u{20ac}\u{20ac}x").is_none());
        assert!(SmtpAddress::extract("ab\u{20ac}\u{20ac}x").is_none());
    }
}
