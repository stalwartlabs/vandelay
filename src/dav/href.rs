/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::hex::decode_pair;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Href(String);

impl Href {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn from_normalised(s: String) -> Href {
        Href(s)
    }
}

impl AsRef<str> for Href {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HrefError {
    #[error("invalid base URL {url:?}: {source}")]
    InvalidBase {
        url: String,
        #[source]
        source: url::ParseError,
    },
    #[error("invalid href {href:?} relative to {base:?}: {source}")]
    InvalidHref {
        href: String,
        base: String,
        #[source]
        source: url::ParseError,
    },
}

pub fn normalise(base: &str, raw: &str) -> Result<Href, HrefError> {
    let base_url = Url::parse(base).map_err(|e| HrefError::InvalidBase {
        url: base.to_owned(),
        source: e,
    })?;
    let resolved = base_url
        .join(raw.trim())
        .map_err(|e| HrefError::InvalidHref {
            href: raw.to_owned(),
            base: base.to_owned(),
            source: e,
        })?;
    Ok(normalise_path(&resolved))
}

pub fn normalise_path(url: &Url) -> Href {
    let mut path = url.path().to_owned();
    if path.is_empty() {
        path.push('/');
    }
    let decoded = percent_decode(&path);
    let reencoded = percent_encode_safe(&decoded);
    Href(reencoded)
}

pub fn absolute_url(base: &str, href: &Href) -> Result<String, HrefError> {
    join_absolute(base, href.as_str())
}

pub fn join_absolute(base: &str, href: &str) -> Result<String, HrefError> {
    let base_url = Url::parse(base).map_err(|e| HrefError::InvalidBase {
        url: base.to_owned(),
        source: e,
    })?;
    let resolved = base_url.join(href).map_err(|e| HrefError::InvalidHref {
        href: href.to_owned(),
        base: base.to_owned(),
        source: e,
    })?;
    Ok(resolved.to_string())
}

pub fn last_path_component(href: &Href) -> String {
    let trimmed = href.as_str().trim_end_matches('/');
    let tail = trimmed.rsplit('/').next().unwrap_or("");
    percent_decode(tail)
}

pub fn parent_collection(href: &Href) -> Href {
    let trimmed = href.as_str().trim_end_matches('/');
    if let Some(slash) = trimmed.rfind('/') {
        let parent = &trimmed[..=slash];
        Href(parent.to_owned())
    } else {
        Href("/".to_owned())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut buf: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = decode_pair(bytes[i + 1], bytes[i + 2])
        {
            buf.push(byte);
            i += 3;
            continue;
        }
        buf.push(bytes[i]);
        i += 1;
    }
    match String::from_utf8(buf) {
        Ok(decoded) => decoded,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

fn percent_encode_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if is_safe(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(upper_hex(byte >> 4));
            out.push(upper_hex(byte & 0x0f));
        }
    }
    out
}

fn is_safe(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z'
        | b'a'..=b'z'
        | b'0'..=b'9'
        | b'-' | b'_' | b'.' | b'~'
        | b'/' | b'@'
    )
}

fn upper_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => '0',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_href_resolves_against_base() {
        let h = normalise("https://dav.example.com/", "/dav/cal/user/default/").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user/default/");
    }

    #[test]
    fn absolute_href_strips_scheme_and_host() {
        let h = normalise(
            "https://dav.example.com/cal/",
            "https://dav.example.com/dav/cal/user/default/event-1.ics",
        )
        .unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user/default/event-1.ics");
    }

    #[test]
    fn percent_encoding_canonicalised_to_uppercase_hex() {
        let h = normalise("https://x/", "/dav/cal/user%2bwork/event%201.ics").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user%2Bwork/event%201.ics");
    }

    #[test]
    fn at_sign_is_preserved_in_collection_paths() {
        let h = normalise("https://x/", "/dav/cal/user@host/default/").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user@host/default/");
    }

    #[test]
    fn trailing_slash_preserved() {
        let coll = normalise("https://x/", "/a/b/").unwrap();
        let item = normalise("https://x/", "/a/b").unwrap();
        assert_eq!(coll.as_str(), "/a/b/");
        assert_eq!(item.as_str(), "/a/b");
        assert_ne!(coll, item);
    }

    #[test]
    fn href_equality_is_byte_exact_after_normalisation() {
        let a = normalise("https://x/", "/Dav/Cal/User/Default/").unwrap();
        let b = normalise("https://x/", "/Dav/Cal/User/Default/").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn last_path_component_decodes() {
        let h = Href("/dav/cal/user/event%201.ics".to_owned());
        assert_eq!(last_path_component(&h), "event 1.ics");
    }

    #[test]
    fn parent_collection_strips_trailing_slash() {
        let h = Href("/dav/cal/user/sub/".to_owned());
        assert_eq!(parent_collection(&h).as_str(), "/dav/cal/user/");
    }

    #[test]
    fn parent_collection_of_file_is_parent_dir() {
        let h = Href("/dav/cal/user/file.ics".to_owned());
        assert_eq!(parent_collection(&h).as_str(), "/dav/cal/user/");
    }

    #[test]
    fn parent_collection_of_root_is_root() {
        let h = Href("/".to_owned());
        assert_eq!(parent_collection(&h).as_str(), "/");
    }

    #[test]
    fn empty_path_treated_as_root() {
        let url = Url::parse("https://dav.example.com").unwrap();
        let h = normalise_path(&url);
        assert_eq!(h.as_str(), "/");
    }

    #[test]
    fn query_and_fragment_stripped() {
        let h = normalise("https://x/", "/dav/cal/?q=1#frag").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/");
    }

    #[test]
    fn unicode_path_round_trips_through_utf8_percent_encoding() {
        let h = normalise("https://x/", "/dav/cal/caf%C3%A9/").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/caf%C3%A9/");
    }
}
