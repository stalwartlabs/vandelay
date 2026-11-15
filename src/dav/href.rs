/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::hex::decode_pair;
use url::Url;

use crate::net::encode::{Percent, SafeSet};

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

impl Href {
    pub fn parse(base: &str, raw: &str) -> Result<Href, HrefError> {
        let resolved = Href::base(base)?
            .join(raw.trim())
            .map_err(|e| HrefError::InvalidHref {
                href: raw.to_owned(),
                base: base.to_owned(),
                source: e,
            })?;
        Ok(Href::from_url(&resolved))
    }

    pub fn from_url(url: &Url) -> Href {
        let path = match url.path() {
            "" => "/",
            path => path,
        };
        let decoded = Href::decode(path);
        Href(Percent::encode(&decoded, SafeSet::Path))
    }

    pub fn to_absolute(&self, base: &str) -> Result<String, HrefError> {
        Href::join_absolute(base, &self.0)
    }

    pub fn join_absolute(base: &str, href: &str) -> Result<String, HrefError> {
        let resolved = Href::base(base)?
            .join(href)
            .map_err(|e| HrefError::InvalidHref {
                href: href.to_owned(),
                base: base.to_owned(),
                source: e,
            })?;
        Ok(resolved.into())
    }

    pub fn last_component(&self) -> String {
        let trimmed = self.0.trim_end_matches('/');
        let tail = trimmed.rsplit_once('/').map_or(trimmed, |(_, tail)| tail);
        Href::decode(tail)
    }

    pub fn parent(&self) -> Href {
        let trimmed = self.0.trim_end_matches('/');
        match trimmed.rsplit_once('/') {
            Some((parent, _)) => {
                let mut out = String::with_capacity(parent.len() + 1);
                out.push_str(parent);
                out.push('/');
                Href(out)
            }
            None => Href("/".to_owned()),
        }
    }

    fn base(base: &str) -> Result<Url, HrefError> {
        Url::parse(base).map_err(|e| HrefError::InvalidBase {
            url: base.to_owned(),
            source: e,
        })
    }

    fn decode(s: &str) -> String {
        let mut buf: Vec<u8> = Vec::with_capacity(s.len());
        let mut rest = s.as_bytes();
        while let [first, tail @ ..] = rest {
            if let (b'%', [high, low, after @ ..]) = (first, tail)
                && let Some(byte) = decode_pair(*high, *low)
            {
                buf.push(byte);
                rest = after;
                continue;
            }
            buf.push(*first);
            rest = tail;
        }
        match String::from_utf8(buf) {
            Ok(decoded) => decoded,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_urls_join_the_base_with_collection_and_item_hrefs() {
        assert_eq!(
            Href::join_absolute("https://x/dav/", "/dav/cal/u/d/").unwrap(),
            "https://x/dav/cal/u/d/"
        );
        let item = Href::parse("https://x/", "/dav/cal/u/d/event.ics").unwrap();
        assert_eq!(
            item.to_absolute("https://x/dav/cal/u/d/").unwrap(),
            "https://x/dav/cal/u/d/event.ics"
        );
    }

    #[test]
    fn relative_href_resolves_against_base() {
        let h = Href::parse("https://dav.example.com/", "/dav/cal/user/default/").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user/default/");
    }

    #[test]
    fn absolute_href_strips_scheme_and_host() {
        let h = Href::parse(
            "https://dav.example.com/cal/",
            "https://dav.example.com/dav/cal/user/default/event-1.ics",
        )
        .unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user/default/event-1.ics");
    }

    #[test]
    fn percent_encoding_canonicalised_to_uppercase_hex() {
        let h = Href::parse("https://x/", "/dav/cal/user%2bwork/event%201.ics").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user%2Bwork/event%201.ics");
    }

    #[test]
    fn at_sign_is_preserved_in_collection_paths() {
        let h = Href::parse("https://x/", "/dav/cal/user@host/default/").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/user@host/default/");
    }

    #[test]
    fn trailing_slash_preserved() {
        let coll = Href::parse("https://x/", "/a/b/").unwrap();
        let item = Href::parse("https://x/", "/a/b").unwrap();
        assert_eq!(coll.as_str(), "/a/b/");
        assert_eq!(item.as_str(), "/a/b");
        assert_ne!(coll, item);
    }

    #[test]
    fn href_equality_is_byte_exact_after_normalisation() {
        let a = Href::parse("https://x/", "/Dav/Cal/User/Default/").unwrap();
        let b = Href::parse("https://x/", "/Dav/Cal/User/Default/").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn last_path_component_decodes() {
        let h = Href("/dav/cal/user/event%201.ics".to_owned());
        assert_eq!(h.last_component(), "event 1.ics");
    }

    #[test]
    fn parent_collection_strips_trailing_slash() {
        let h = Href("/dav/cal/user/sub/".to_owned());
        assert_eq!(h.parent().as_str(), "/dav/cal/user/");
    }

    #[test]
    fn parent_collection_of_file_is_parent_dir() {
        let h = Href("/dav/cal/user/file.ics".to_owned());
        assert_eq!(h.parent().as_str(), "/dav/cal/user/");
    }

    #[test]
    fn parent_collection_of_root_is_root() {
        let h = Href("/".to_owned());
        assert_eq!(h.parent().as_str(), "/");
    }

    #[test]
    fn empty_path_treated_as_root() {
        let url = Url::parse("https://dav.example.com").unwrap();
        let h = Href::from_url(&url);
        assert_eq!(h.as_str(), "/");
    }

    #[test]
    fn query_and_fragment_stripped() {
        let h = Href::parse("https://x/", "/dav/cal/?q=1#frag").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/");
    }

    #[test]
    fn unicode_path_round_trips_through_utf8_percent_encoding() {
        let h = Href::parse("https://x/", "/dav/cal/caf%C3%A9/").unwrap();
        assert_eq!(h.as_str(), "/dav/cal/caf%C3%A9/");
    }
}
