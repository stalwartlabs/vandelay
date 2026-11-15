/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::dav::client::{DavClient, MultiStatus};
use crate::dav::error::DavError;
use crate::dav::href::{Href, HrefError};
use crate::dav::parse::ResourceProps;
use crate::dav::xml::Propfind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DavKind {
    Caldav,
    Carddav,
    Webdav,
}

impl DavKind {
    pub fn well_known_uri(self) -> Option<&'static str> {
        match self {
            DavKind::Caldav => Some("/.well-known/caldav"),
            DavKind::Carddav => Some("/.well-known/carddav"),
            DavKind::Webdav => None,
        }
    }

    pub fn home_set_body(self) -> Option<&'static str> {
        match self {
            DavKind::Caldav => Some(Propfind::CalendarHomeSet.body()),
            DavKind::Carddav => Some(Propfind::AddressbookHomeSet.body()),
            DavKind::Webdav => None,
        }
    }

    pub fn principal_and_home_set_body(self) -> Option<&'static str> {
        match self {
            DavKind::Caldav => Some(Propfind::PrincipalAndCalendarHomeSet.body()),
            DavKind::Carddav => Some(Propfind::PrincipalAndAddressbookHomeSet.body()),
            DavKind::Webdav => None,
        }
    }

    pub fn collection_listing_body(self) -> &'static str {
        match self {
            DavKind::Caldav => Propfind::CalendarCollections.body(),
            DavKind::Carddav => Propfind::AddressbookCollections.body(),
            DavKind::Webdav => Propfind::WebdavListing.body(),
        }
    }

    pub fn extract_home_set(self, props: &ResourceProps) -> Option<&str> {
        match self {
            DavKind::Caldav => props.calendar_home_set.as_deref(),
            DavKind::Carddav => props.addressbook_home_set.as_deref(),
            DavKind::Webdav => None,
        }
    }

    pub fn item_resourcetype_matches(self, props: &ResourceProps) -> bool {
        match self {
            DavKind::Caldav => props.is_calendar,
            DavKind::Carddav => props.is_addressbook,
            DavKind::Webdav => props.is_collection,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredCollection {
    pub url: String,
    pub href: Href,
    pub props: ResourceProps,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("transport: {0}")]
    Transport(#[from] DavError),
    #[error("href: {0}")]
    Href(#[from] HrefError),
    #[error("no DAV collections found under {url}")]
    NotFound { url: String },
}

#[derive(Debug, Clone)]
pub struct Discovery {
    pub principal_url: Option<String>,
    pub home_set_url: String,
    pub collections: Vec<DiscoveredCollection>,
}

impl Discovery {
    pub fn run(
        client: &DavClient,
        kind: DavKind,
        user_url: &str,
    ) -> Result<Discovery, DiscoveryError> {
        let probe = Probe { client, kind };
        if let Some(disc) = probe.home_or_collection(user_url)? {
            return Ok(disc);
        }
        if let Some(disc) = probe.via_principal(user_url)? {
            return Ok(disc);
        }
        if let Some(well_known) = kind.well_known_uri() {
            let absolute = Href::join_absolute(user_url, well_known)?;
            if let Some(disc) = probe.via_principal(&absolute)? {
                return Ok(disc);
            }
            if let Some(disc) = probe.home_or_collection(&absolute)? {
                return Ok(disc);
            }
        }
        Err(DiscoveryError::NotFound {
            url: user_url.to_owned(),
        })
    }
}

#[derive(Clone, Copy)]
struct Probe<'a> {
    client: &'a DavClient,
    kind: DavKind,
}

impl Probe<'_> {
    fn is_dead_end(err: &DavError) -> bool {
        match err {
            DavError::HttpStatus { status, .. } => (400..600).contains(status),
            DavError::RetriesExhausted(_) | DavError::Multistatus(_) => true,
            _ => false,
        }
    }

    fn propfind(&self, url: &str, body: &str) -> Result<Option<MultiStatus>, DiscoveryError> {
        match self.client.propfind_responses(url, 0, body, url) {
            Ok(ms) => Ok(Some(ms)),
            Err(e) if Probe::is_dead_end(&e) => Ok(None),
            Err(e) => Err(DiscoveryError::Transport(e)),
        }
    }

    fn home_or_collection(&self, url: &str) -> Result<Option<Discovery>, DiscoveryError> {
        let collections = match self.collections(url) {
            Ok(c) => c,
            Err(DiscoveryError::Transport(e)) if Probe::is_dead_end(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        if collections.is_empty() {
            return Ok(None);
        }
        let principal_url = self.principal_url(url)?;
        Ok(Some(Discovery {
            principal_url,
            home_set_url: url.to_owned(),
            collections,
        }))
    }

    fn principal_url(&self, url: &str) -> Result<Option<String>, DiscoveryError> {
        if self.kind == DavKind::Webdav {
            return Ok(None);
        }
        let Some(ms) = self.propfind(url, Propfind::CurrentUserPrincipal.body())? else {
            return Ok(None);
        };
        let Some(principal_href) = ms
            .responses
            .iter()
            .find_map(|r| r.props.current_user_principal.as_deref())
        else {
            return Ok(None);
        };
        let resolved = Href::parse(&ms.final_url, principal_href)?.to_absolute(&ms.final_url)?;
        Ok(Some(resolved))
    }

    fn via_principal(&self, url: &str) -> Result<Option<Discovery>, DiscoveryError> {
        let kind = self.kind;
        let Some(combined_body) = kind.principal_and_home_set_body() else {
            return Ok(None);
        };
        let Some(first_ms) = self.propfind(url, combined_body)? else {
            return Ok(None);
        };
        let first_final_url = first_ms.final_url.as_str();
        let direct_home_set = first_ms
            .responses
            .iter()
            .find_map(|r| kind.extract_home_set(&r.props));
        let (principal_url, home_set_url) = if let Some(home) = direct_home_set {
            let resolved = Href::parse(first_final_url, home)?.to_absolute(first_final_url)?;
            (first_final_url.to_owned(), resolved)
        } else {
            let principal_href = first_ms
                .responses
                .iter()
                .find_map(|r| r.props.current_user_principal.as_deref());
            let principal_url = match principal_href {
                Some(rel) => Href::parse(first_final_url, rel)?.to_absolute(first_final_url)?,
                None => first_final_url.to_owned(),
            };
            let Some(home_set_body) = kind.home_set_body() else {
                return Ok(None);
            };
            let Some(home_ms) = self.propfind(&principal_url, home_set_body)? else {
                return Ok(None);
            };
            let principal_after_redirect = home_ms.final_url.as_str();
            let Some(home_set_rel) = home_ms
                .responses
                .iter()
                .find_map(|r| kind.extract_home_set(&r.props))
            else {
                return Ok(None);
            };
            let resolved = Href::parse(principal_after_redirect, home_set_rel)?
                .to_absolute(principal_after_redirect)?;
            (principal_url, resolved)
        };
        let collections = self.collections(&home_set_url)?;
        if collections.is_empty() {
            return Ok(None);
        }
        Ok(Some(Discovery {
            principal_url: Some(principal_url),
            home_set_url,
            collections,
        }))
    }

    fn collections(&self, url: &str) -> Result<Vec<DiscoveredCollection>, DiscoveryError> {
        let kind = self.kind;
        let responses = self
            .client
            .propfind_responses(url, 1, kind.collection_listing_body(), url)?
            .responses;
        let self_href = Href::parse(url, "").map(Href::into_string).ok();
        let mut out: Vec<DiscoveredCollection> = Vec::new();
        for r in responses {
            if kind == DavKind::Webdav {
                let is_self = self_href.as_deref() == Some(r.href.as_str());
                if !r.props.is_collection || !is_self {
                    continue;
                }
            } else {
                if !kind.item_resourcetype_matches(&r.props) {
                    continue;
                }
                if Probe::supports_only_vfreebusy(&r.props) {
                    self.client.logger().warn(format_args!(
                        "calendar {} supports only VFREEBUSY; skipped",
                        r.href.as_str(),
                    ));
                    continue;
                }
            }
            out.push(DiscoveredCollection {
                url: r.href.to_absolute(url)?,
                href: r.href,
                props: r.props,
            });
        }
        Ok(out)
    }

    fn supports_only_vfreebusy(props: &ResourceProps) -> bool {
        props.is_calendar
            && !props.supported_components.is_empty()
            && !props
                .supported_components
                .iter()
                .any(|c| hashify::set!(c.as_bytes(), "VEVENT", "VTODO", "VJOURNAL"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_uris_match_rfc_6764() {
        assert_eq!(
            DavKind::Caldav.well_known_uri(),
            Some("/.well-known/caldav")
        );
        assert_eq!(
            DavKind::Carddav.well_known_uri(),
            Some("/.well-known/carddav")
        );
        assert_eq!(DavKind::Webdav.well_known_uri(), None);
    }

    #[test]
    fn home_set_body_only_for_calendar_addressbook() {
        assert!(DavKind::Caldav.home_set_body().is_some());
        assert!(DavKind::Carddav.home_set_body().is_some());
        assert!(DavKind::Webdav.home_set_body().is_none());
    }

    #[test]
    fn item_resourcetype_matches_calendar_for_caldav() {
        let p = ResourceProps {
            is_calendar: true,
            ..Default::default()
        };
        assert!(DavKind::Caldav.item_resourcetype_matches(&p));
        assert!(!DavKind::Carddav.item_resourcetype_matches(&p));
    }

    #[test]
    fn item_resourcetype_matches_collection_for_webdav() {
        let p = ResourceProps {
            is_collection: true,
            ..Default::default()
        };
        assert!(DavKind::Webdav.item_resourcetype_matches(&p));
    }
}
