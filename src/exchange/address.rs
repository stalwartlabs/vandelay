/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

const ATEXT_SYMBOLS: &[u8] = b"!#$%&'*+-/=?^_`{|}~";
const MAX_LOCAL_PART: usize = 64;
const MAX_DOMAIN: usize = 255;
const MAX_LABEL: usize = 63;

pub fn as_smtp_address(raw: &str) -> Option<String> {
    let candidate = normalise(raw)?;
    is_addr_spec(&candidate).then_some(candidate)
}

pub fn extract_smtp_address(raw: &str) -> Option<String> {
    let candidate = normalise(raw)?;
    if !is_addr_spec(&candidate) {
        return None;
    }
    let domain = candidate.split('@').next_back()?;
    psl::suffix(domain.as_bytes())
        .is_some_and(|s| s.is_known())
        .then_some(candidate)
}

fn normalise(raw: &str) -> Option<String> {
    let mut value = raw.trim();
    if let Some(open) = value.rfind('<')
        && let Some(close) = value.rfind('>')
        && open < close
    {
        value = value[open + 1..close].trim();
    }
    for prefix in ["mailto:", "smtp:"] {
        if value.len() > prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix) {
            value = value[prefix.len()..].trim();
        }
    }
    (!value.is_empty()).then(|| value.to_owned())
}

fn is_addr_spec(value: &str) -> bool {
    if !value.is_ascii() || value.starts_with('/') {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !domain.contains('@') && is_local_part(local) && is_domain(domain)
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

#[cfg(test)]
mod tests {
    use super::*;

    const EXCHANGE_DN: &str = "/o=ExchangeLabs/ou=Exchange Administrative Group \
         (FYDIBOHF23SPDLT)/cn=Recipients/cn=bdc77b18152647a29d28ce1188376dc9-kristina";

    #[test]
    fn plain_addresses_are_accepted() {
        assert_eq!(
            as_smtp_address("user@example.com").as_deref(),
            Some("user@example.com")
        );
        assert_eq!(
            as_smtp_address("  First.Last@sub.example.co.uk  ").as_deref(),
            Some("First.Last@sub.example.co.uk")
        );
    }

    #[test]
    fn internal_only_tlds_are_kept_when_the_server_says_smtp() {
        assert_eq!(
            as_smtp_address("user@contoso.local").as_deref(),
            Some("user@contoso.local")
        );
        assert!(
            extract_smtp_address("user@contoso.local").is_none(),
            "an unknown suffix is not enough to override the routing type"
        );
    }

    #[test]
    fn routing_prefixes_and_display_names_are_stripped() {
        assert_eq!(
            as_smtp_address("SMTP:user@example.com").as_deref(),
            Some("user@example.com")
        );
        assert_eq!(
            as_smtp_address("mailto:user@example.com").as_deref(),
            Some("user@example.com")
        );
        assert_eq!(
            as_smtp_address("Jane Doe <jane@example.com>").as_deref(),
            Some("jane@example.com")
        );
        assert_eq!(
            extract_smtp_address("SMTP:jane@example.com").as_deref(),
            Some("jane@example.com")
        );
    }

    #[test]
    fn entra_guest_upns_are_accepted() {
        let guest = "bob_contoso.com#EXT#@fabrikam.onmicrosoft.com";
        assert_eq!(as_smtp_address(guest).as_deref(), Some(guest));
        assert_eq!(extract_smtp_address(guest).as_deref(), Some(guest));
    }

    #[test]
    fn legacy_exchange_distinguished_names_are_rejected() {
        assert!(as_smtp_address(EXCHANGE_DN).is_none());
        assert!(extract_smtp_address(EXCHANGE_DN).is_none());
        assert!(
            as_smtp_address(
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
            assert!(as_smtp_address(bad).is_none(), "must reject {bad:?}");
            assert!(extract_smtp_address(bad).is_none(), "must reject {bad:?}");
        }
    }

    #[test]
    fn over_long_parts_are_rejected() {
        let long_local = format!("{}@example.com", "a".repeat(65));
        assert!(as_smtp_address(&long_local).is_none());
        let long_label = format!("user@{}.com", "a".repeat(64));
        assert!(as_smtp_address(&long_label).is_none());
    }

    #[test]
    fn dotless_domains_are_kept_when_the_server_says_smtp() {
        assert_eq!(
            as_smtp_address("postmaster@localhost").as_deref(),
            Some("postmaster@localhost"),
            "RFC 5321 allows a bare domain and old on-prem setups use them"
        );
        assert!(extract_smtp_address("postmaster@localhost").is_none());
    }

    #[test]
    fn public_suffix_is_required_only_by_the_strict_extractor() {
        assert!(extract_smtp_address("user@example.com").is_some());
        assert!(extract_smtp_address("user@example.co.uk").is_some());
        assert!(extract_smtp_address("user@example.invalidtld").is_none());
        assert!(as_smtp_address("user@example.invalidtld").is_some());
    }
}
