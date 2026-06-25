/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::dav::client::DavClient;
use crate::dav::href::{Href, absolute_url, normalise};
use crate::dav::parse::DavResponse;
use crate::dav::xml;
use crate::jmap::error::JmapError;

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

    pub fn home_set_body(self) -> Option<String> {
        match self {
            DavKind::Caldav => Some(xml::propfind_calendar_home_set()),
            DavKind::Carddav => Some(xml::propfind_addressbook_home_set()),
            DavKind::Webdav => None,
        }
    }

    pub fn principal_and_home_set_body(self) -> Option<String> {
        match self {
            DavKind::Caldav => Some(xml::propfind_principal_and_calendar_home_set()),
            DavKind::Carddav => Some(xml::propfind_principal_and_addressbook_home_set()),
            DavKind::Webdav => None,
        }
    }

    pub fn collection_listing_body(self) -> String {
        match self {
            DavKind::Caldav => xml::propfind_calendar_collections(),
            DavKind::Carddav => xml::propfind_addressbook_collections(),
            DavKind::Webdav => xml::propfind_webdav_listing(),
        }
    }

    pub fn extract_home_set(self, props: &crate::dav::parse::ResourceProps) -> Option<&str> {
        match self {
            DavKind::Caldav => props.calendar_home_set.as_deref(),
            DavKind::Carddav => props.addressbook_home_set.as_deref(),
            DavKind::Webdav => None,
        }
    }

    pub fn item_resourcetype_matches(self, props: &crate::dav::parse::ResourceProps) -> bool {
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
    pub props: crate::dav::parse::ResourceProps,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("transport: {0}")]
    Transport(#[from] JmapError),
    #[error("xml: {0}")]
    Parse(#[from] crate::dav::parse::ParseError),
    #[error("href: {0}")]
    Href(#[from] crate::dav::href::HrefError),
    #[error("no DAV collections found under {url}")]
    NotFound { url: String },
}

#[derive(Debug, Clone)]
pub struct Discovery {
    pub principal_url: Option<String>,
    pub home_set_url: String,
    pub collections: Vec<DiscoveredCollection>,
}

pub fn discover(
    client: &DavClient,
    kind: DavKind,
    user_url: &str,
) -> Result<Discovery, DiscoveryError> {
    if let Some(disc) = try_treat_url_as_home_or_collection(client, kind, user_url)? {
        return Ok(disc);
    }
    if let Some(disc) = try_via_principal(client, kind, user_url)? {
        return Ok(disc);
    }
    if let Some(well_known) = kind.well_known_uri() {
        let absolute = absolute_url(user_url, &Href::from_normalised(well_known.to_owned()))?;
        if let Some(disc) = try_via_principal(client, kind, &absolute)? {
            return Ok(disc);
        }
        if let Some(disc) = try_treat_url_as_home_or_collection(client, kind, &absolute)? {
            return Ok(disc);
        }
    }
    Err(DiscoveryError::NotFound {
        url: user_url.to_owned(),
    })
}

fn try_treat_url_as_home_or_collection(
    client: &DavClient,
    kind: DavKind,
    url: &str,
) -> Result<Option<Discovery>, DiscoveryError> {
    let body = kind.collection_listing_body();
    let collections = match propfind_collections(client, kind, url, &body) {
        Ok(c) => c,
        Err(DiscoveryError::Transport(JmapError::HttpStatus { status, .. }))
            if (400..500).contains(&status) =>
        {
            return Ok(None);
        }
        Err(DiscoveryError::Transport(JmapError::Malformed(_)))
        | Err(DiscoveryError::Transport(JmapError::RetriesExhausted(_))) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if collections.is_empty() {
        return Ok(None);
    }
    let principal_url = resolve_principal_url(client, kind, url)?;
    Ok(Some(Discovery {
        principal_url,
        home_set_url: url.to_owned(),
        collections,
    }))
}

fn resolve_principal_url(
    client: &DavClient,
    kind: DavKind,
    url: &str,
) -> Result<Option<String>, DiscoveryError> {
    if matches!(kind, DavKind::Webdav) {
        return Ok(None);
    }
    let body = xml::propfind_current_user_principal();
    let ms = match client.propfind_responses(url, 0, &body, url) {
        Ok(ms) => ms,
        Err(JmapError::HttpStatus { status, .. }) if (400..600).contains(&status) => {
            return Ok(None);
        }
        Err(JmapError::RetriesExhausted(_)) | Err(JmapError::Malformed(_)) => return Ok(None),
        Err(e) => return Err(DiscoveryError::Transport(e)),
    };
    let final_url = ms.final_url.clone();
    let Some(principal_href) = ms
        .responses
        .iter()
        .find_map(|r| r.props.current_user_principal.as_deref())
    else {
        return Ok(None);
    };
    let resolved = absolute_url(&final_url, &normalise(&final_url, principal_href)?)?;
    Ok(Some(resolved))
}

fn try_via_principal(
    client: &DavClient,
    kind: DavKind,
    url: &str,
) -> Result<Option<Discovery>, DiscoveryError> {
    let Some(combined_body) = kind.principal_and_home_set_body() else {
        return Ok(None);
    };
    let first_ms = match client.propfind_responses(url, 0, &combined_body, url) {
        Ok(ms) => ms,
        Err(JmapError::HttpStatus { status, .. }) if (400..600).contains(&status) => {
            return Ok(None);
        }
        Err(JmapError::RetriesExhausted(_)) | Err(JmapError::Malformed(_)) => return Ok(None),
        Err(e) => return Err(DiscoveryError::Transport(e)),
    };
    let first_final_url = first_ms.final_url.clone();
    let direct_home_set = first_ms
        .responses
        .iter()
        .find_map(|r| kind.extract_home_set(&r.props))
        .map(|h| h.to_owned());
    let (principal_url, home_set_url) = if let Some(home) = direct_home_set {
        let resolved = absolute_url(&first_final_url, &normalise(&first_final_url, &home)?)?;
        (first_final_url.clone(), resolved)
    } else {
        let principal_href = first_ms
            .responses
            .iter()
            .find_map(|r| r.props.current_user_principal.as_deref())
            .map(|h| h.to_owned());
        let principal_url = match principal_href {
            Some(rel) => absolute_url(&first_final_url, &normalise(&first_final_url, &rel)?)?,
            None => first_final_url.clone(),
        };
        let Some(home_set_body) = kind.home_set_body() else {
            return Ok(None);
        };
        let home_ms =
            match client.propfind_responses(&principal_url, 0, &home_set_body, &principal_url) {
                Ok(ms) => ms,
                Err(JmapError::HttpStatus { status, .. }) if (400..600).contains(&status) => {
                    return Ok(None);
                }
                Err(JmapError::RetriesExhausted(_)) | Err(JmapError::Malformed(_)) => {
                    return Ok(None);
                }
                Err(e) => return Err(DiscoveryError::Transport(e)),
            };
        let principal_after_redirect = home_ms.final_url.clone();
        let home_set_href = home_ms
            .responses
            .iter()
            .find_map(|r| kind.extract_home_set(&r.props))
            .map(|h| h.to_owned());
        let Some(home_set_rel) = home_set_href else {
            return Ok(None);
        };
        let resolved = absolute_url(
            &principal_after_redirect,
            &normalise(&principal_after_redirect, &home_set_rel)?,
        )?;
        (principal_url, resolved)
    };
    let listing_body = kind.collection_listing_body();
    let collections = propfind_collections(client, kind, &home_set_url, &listing_body)?;
    if collections.is_empty() {
        return Ok(None);
    }
    Ok(Some(Discovery {
        principal_url: Some(principal_url),
        home_set_url,
        collections,
    }))
}

fn propfind_collections(
    client: &DavClient,
    kind: DavKind,
    url: &str,
    body: &str,
) -> Result<Vec<DiscoveredCollection>, DiscoveryError> {
    let responses = propfind_depth(client, url, 1, body)?;
    let self_href = normalise(url, "").map(|h| h.into_string()).ok();
    let mut out: Vec<DiscoveredCollection> = Vec::new();
    for r in responses {
        let is_self = self_href.as_deref() == Some(r.href.as_str());
        if matches!(kind, DavKind::Webdav) {
            if !r.props.is_collection {
                continue;
            }
            if !is_self {
                continue;
            }
        } else {
            if !kind.item_resourcetype_matches(&r.props) {
                continue;
            }
            if calendar_supports_only_vfreebusy(&r.props) {
                client.logger().warn(&format!(
                    "calendar {} supports only {:?}; skipping",
                    r.href.as_str(),
                    r.props.supported_components,
                ));
                continue;
            }
        }
        let abs = absolute_url(url, &r.href)?;
        out.push(DiscoveredCollection {
            url: abs,
            href: r.href.clone(),
            props: r.props,
        });
    }
    Ok(out)
}

fn calendar_supports_only_vfreebusy(props: &crate::dav::parse::ResourceProps) -> bool {
    if !props.is_calendar {
        return false;
    }
    if props.supported_components.is_empty() {
        return false;
    }
    let has_useful = props
        .supported_components
        .iter()
        .any(|c| matches!(c.as_str(), "VEVENT" | "VTODO" | "VJOURNAL"));
    !has_useful
}

fn propfind_depth(
    client: &DavClient,
    url: &str,
    depth: u8,
    body: &str,
) -> Result<Vec<DavResponse>, DiscoveryError> {
    let ms = client.propfind_responses(url, depth, body, url)?;
    Ok(ms.responses)
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
        let p = crate::dav::parse::ResourceProps {
            is_calendar: true,
            ..Default::default()
        };
        assert!(DavKind::Caldav.item_resourcetype_matches(&p));
        assert!(!DavKind::Carddav.item_resourcetype_matches(&p));
    }

    #[test]
    fn item_resourcetype_matches_collection_for_webdav() {
        let p = crate::dav::parse::ResourceProps {
            is_collection: true,
            ..Default::default()
        };
        assert!(DavKind::Webdav.item_resourcetype_matches(&p));
    }
}
