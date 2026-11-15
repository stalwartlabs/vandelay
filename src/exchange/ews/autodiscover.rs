/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::{self, Write};
use std::mem;

use quick_xml::NsReader;
use quick_xml::events::Event;
use serde_json::Value;
use url::Url;

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::srv::SrvRecord;
use crate::net::encode::{Percent, SafeSet};
use crate::net::http::{AgentProfile, ClientOptions, HttpCore, Methods, Redirects};
use crate::net::xml::{Entity, XmlEscape};

const V2_HOST: &str = "https://outlook.office365.com";
const EWS_PATH: &str = "/EWS/Exchange.asmx";
const POX_REQ_NS: &str =
    "http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006";
const POX_RESP_NS: &str =
    "http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a";
const POX_MAX_BODY: u64 = 2 * 1024 * 1024;
const HTTPS_PORT: u16 = 443;
const MAX_ADDRESS_REDIRECTS: u8 = 4;
const MAX_URL_REDIRECTS: u8 = 1;
const PROFILE: AgentProfile = AgentProfile {
    redirects: Redirects::Follow,
    methods: Methods::Standard,
    max_body: POX_MAX_BODY,
};

#[derive(Debug, Clone)]
pub struct DiscoveryResult {
    pub ews_url: String,
    pub source: DiscoverySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverySource {
    V2,
    V1,
    SuppliedUrl,
}

impl fmt::Display for DiscoverySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DiscoverySource::V2 => "found by Autodiscover v2",
            DiscoverySource::V1 => "found by Autodiscover v1",
            DiscoverySource::SuppliedUrl => "from --url",
        })
    }
}

#[derive(Debug, Clone)]
enum PoxOutcome {
    EwsUrl(String),
    RedirectAddr(String),
    RedirectUrl(String),
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoxElement {
    Protocol,
    Type,
    EwsUrl,
    Action,
    RedirectAddr,
    RedirectUrl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoxField {
    Type,
    EwsUrl,
    Action,
    RedirectAddr,
    RedirectUrl,
}

pub struct Autodiscover {
    core: HttpCore,
}

impl Autodiscover {
    pub fn new(options: ClientOptions) -> Autodiscover {
        Autodiscover {
            core: HttpCore::new(options, PROFILE),
        }
    }

    pub fn is_fully_qualified_ews_url(url: &str) -> bool {
        Url::parse(url)
            .is_ok_and(|u| u.scheme() == "https" && u.path().eq_ignore_ascii_case(EWS_PATH))
    }

    pub fn discover(
        &self,
        supplied_url: Option<&str>,
        email: Option<&str>,
        auth_header: Option<&str>,
    ) -> Result<DiscoveryResult, EwsError> {
        if let Some(url) = supplied_url
            && Autodiscover::is_fully_qualified_ews_url(url)
        {
            return Ok(DiscoveryResult {
                ews_url: url.to_owned(),
                source: DiscoverySource::SuppliedUrl,
            });
        }
        let Some(email) = email else {
            return Err(EwsError::AutodiscoverFailed(
                "either a fully-qualified --url or --mailbox is required".to_owned(),
            ));
        };
        if let Ok(url) = self.v2(email) {
            return Ok(DiscoveryResult {
                ews_url: url,
                source: DiscoverySource::V2,
            });
        }
        let mut tried: usize = 0;
        let mut redirects: u8 = 0;
        let mut url_redirects: u8 = 0;
        let mut current_email = email.to_owned();
        'outer: loop {
            let domain = current_email.split('@').nth(1).ok_or_else(|| {
                EwsError::AutodiscoverFailed("--mailbox is not an email".to_owned())
            })?;
            for candidate in Autodiscover::pox_candidates(domain) {
                tried += 1;
                match self.v1(&candidate, &current_email, auth_header) {
                    Ok(PoxOutcome::EwsUrl(url)) => {
                        return Ok(DiscoveryResult {
                            ews_url: url,
                            source: DiscoverySource::V1,
                        });
                    }
                    Ok(PoxOutcome::RedirectAddr(addr)) => {
                        if redirects >= MAX_ADDRESS_REDIRECTS {
                            return Err(EwsError::AutodiscoverLoop);
                        }
                        redirects += 1;
                        current_email = addr;
                        continue 'outer;
                    }
                    Ok(PoxOutcome::RedirectUrl(url)) => {
                        if url_redirects >= MAX_URL_REDIRECTS {
                            return Err(EwsError::AutodiscoverLoop);
                        }
                        url_redirects += 1;
                        tried += 1;
                        if let Ok(PoxOutcome::EwsUrl(u)) =
                            self.v1(&url, &current_email, auth_header)
                        {
                            return Ok(DiscoveryResult {
                                ews_url: u,
                                source: DiscoverySource::V1,
                            });
                        }
                    }
                    Err(_) | Ok(PoxOutcome::Failed) => {}
                }
            }
            break;
        }
        if let Some(url) = supplied_url {
            return Ok(DiscoveryResult {
                ews_url: url.to_owned(),
                source: DiscoverySource::SuppliedUrl,
            });
        }
        Err(EwsError::AutodiscoverFailed(format!(
            "tried {tried} candidate(s), none returned a v1 EwsUrl"
        )))
    }

    fn v2(&self, email: &str) -> Result<String, EwsError> {
        let mut url = format!("{V2_HOST}/autodiscover/autodiscover.json?Email=");
        Percent::encode_into(&mut url, email, SafeSet::Unreserved);
        url.push_str("&Protocol=Ews");
        let mut resp = self
            .core
            .agent()
            .get(&url)
            .header("Accept", "application/json")
            .call()
            .map_err(|e| EwsError::Transport(format!("autodiscover v2: {e}")))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(EwsError::AutodiscoverFailed(format!(
                "v2 returned http {status}"
            )));
        }
        let body: Value = resp
            .body_mut()
            .read_json()
            .map_err(|e| EwsError::Malformed(format!("v2 body: {e}")))?;
        let protocol = body
            .get("Protocol")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !protocol.eq_ignore_ascii_case("Ews") {
            return Err(EwsError::AutodiscoverFailed(format!(
                "v2 returned protocol {protocol:?}"
            )));
        }
        body.get("Url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| EwsError::AutodiscoverFailed("v2 missing Url".to_owned()))
    }

    fn pox_candidates(domain: &str) -> Vec<String> {
        let mut out = vec![
            format!("https://autodiscover.{domain}/autodiscover/autodiscover.xml"),
            format!("https://{domain}/autodiscover/autodiscover.xml"),
        ];
        out.extend(
            SrvRecord::lookup(&format!("_autodiscover._tcp.{domain}"))
                .into_iter()
                .map(|SrvRecord { target, port, .. }| {
                    let mut url = format!("https://{}", target.trim_end_matches('.'));
                    if port != HTTPS_PORT {
                        let _ = write!(url, ":{port}");
                    }
                    url.push_str("/autodiscover/autodiscover.xml");
                    url
                }),
        );
        out
    }

    fn v1(
        &self,
        url: &str,
        email: &str,
        auth_header: Option<&str>,
    ) -> Result<PoxOutcome, EwsError> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <Autodiscover xmlns=\"{POX_REQ_NS}\">\
             <Request><EMailAddress>{}</EMailAddress>\
             <AcceptableResponseSchema>{POX_RESP_NS}</AcceptableResponseSchema>\
             </Request></Autodiscover>",
            XmlEscape(email)
        );
        let mut req = self
            .core
            .agent()
            .post(url)
            .header("Content-Type", "text/xml; charset=utf-8");
        if let Some(h) = auth_header {
            req = req.header("Authorization", h);
        }
        let mut resp = req
            .send(body.as_bytes())
            .map_err(|e| EwsError::Transport(format!("v1 POST: {e}")))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(EwsError::AutodiscoverFailed(format!(
                "{url} returned http {status}"
            )));
        }
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(POX_MAX_BODY)
            .read_to_vec()
            .map_err(|e| EwsError::Malformed(format!("v1 body: {e}")))?;
        PoxOutcome::parse(&bytes)
    }
}

impl PoxElement {
    fn of(local: &str) -> Option<PoxElement> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            PoxElement,
            "Protocol" => PoxElement::Protocol,
            "Type" => PoxElement::Type,
            "EwsUrl" => PoxElement::EwsUrl,
            "Action" => PoxElement::Action,
            "RedirectAddr" => PoxElement::RedirectAddr,
            "RedirectUrl" => PoxElement::RedirectUrl,
        )
        .copied()
    }
}

impl PoxOutcome {
    fn parse(body: &[u8]) -> Result<PoxOutcome, EwsError> {
        let mut xml = NsReader::from_reader(body);
        xml.config_mut().trim_text(true);
        let mut buf = Vec::new();
        let mut current: Option<PoxField> = None;
        let mut current_type: Option<String> = None;
        let mut ews_url: Option<String> = None;
        let mut redirect_addr: Option<String> = None;
        let mut redirect_url: Option<String> = None;
        let mut action: Option<String> = None;
        let mut cur = String::new();
        loop {
            buf.clear();
            let (_, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(e) => {
                    cur.clear();
                    current = match PoxElement::of(e.local_name().into_inner()) {
                        Some(PoxElement::Protocol) => {
                            current_type = None;
                            None
                        }
                        Some(PoxElement::Type) => Some(PoxField::Type),
                        Some(PoxElement::EwsUrl) => {
                            matches!(current_type.as_deref(), Some("EXPR" | "EXCH"))
                                .then_some(PoxField::EwsUrl)
                        }
                        Some(PoxElement::Action) => Some(PoxField::Action),
                        Some(PoxElement::RedirectAddr) => Some(PoxField::RedirectAddr),
                        Some(PoxElement::RedirectUrl) => Some(PoxField::RedirectUrl),
                        None => None,
                    };
                }
                Event::End(e) => {
                    if let Some(field) = current.take() {
                        let text = Some(mem::take(&mut cur));
                        match field {
                            PoxField::Type => current_type = text,
                            PoxField::EwsUrl => ews_url = text,
                            PoxField::Action => action = text,
                            PoxField::RedirectAddr => redirect_addr = text,
                            PoxField::RedirectUrl => redirect_url = text,
                        }
                    }
                    cur.clear();
                    if PoxElement::of(e.local_name().into_inner()) == Some(PoxElement::Protocol) {
                        current_type = None;
                    }
                }
                Event::Text(t) => cur.push_str(&t),
                Event::CData(c) => cur.push_str(&c),
                Event::GeneralRef(g) => {
                    if let Some(c) = Entity::from_ref(&g) {
                        cur.push(c);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if let Some(addr) = redirect_addr.filter(|_| action.as_deref() == Some("redirectAddr")) {
            return Ok(PoxOutcome::RedirectAddr(addr));
        }
        if let Some(url) = redirect_url.filter(|_| action.as_deref() == Some("redirectUrl")) {
            return Ok(PoxOutcome::RedirectUrl(url));
        }
        Ok(ews_url.map_or(PoxOutcome::Failed, PoxOutcome::EwsUrl))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The discovery source reads as a phrase after the endpoint URL.
    #[test]
    fn discovery_source_names_how_the_endpoint_was_found() {
        assert_eq!(DiscoverySource::V2.to_string(), "found by Autodiscover v2");
        assert_eq!(DiscoverySource::V1.to_string(), "found by Autodiscover v1");
        assert_eq!(DiscoverySource::SuppliedUrl.to_string(), "from --url");
    }

    #[test]
    fn detects_fully_qualified_ews_url() {
        assert!(Autodiscover::is_fully_qualified_ews_url(
            "https://outlook.office365.com/EWS/Exchange.asmx"
        ));
        assert!(Autodiscover::is_fully_qualified_ews_url(
            "https://exchange.example.com/EWS/Exchange.asmx"
        ));
        assert!(!Autodiscover::is_fully_qualified_ews_url(
            "http://outlook.office365.com/EWS/Exchange.asmx"
        ));
        assert!(!Autodiscover::is_fully_qualified_ews_url(
            "https://outlook.office365.com/"
        ));
    }

    #[test]
    fn pox_response_extracts_ews_url() {
        let body = format!(
            "<?xml version=\"1.0\"?>\
             <Autodiscover xmlns=\"{POX_RESP_NS}\">\
             <Response><Account><Protocol><Type>EXPR</Type><EwsUrl>https://srv/EWS/Exchange.asmx</EwsUrl></Protocol></Account></Response></Autodiscover>"
        );
        match PoxOutcome::parse(body.as_bytes()).unwrap() {
            PoxOutcome::EwsUrl(u) => assert_eq!(u, "https://srv/EWS/Exchange.asmx"),
            _ => panic!("expected EwsUrl"),
        }
    }

    #[test]
    fn pox_response_handles_redirect_addr() {
        let body = format!(
            "<?xml version=\"1.0\"?>\
             <Autodiscover xmlns=\"{POX_RESP_NS}\">\
             <Response><Account><Action>redirectAddr</Action><RedirectAddr>bob@other.com</RedirectAddr></Account></Response></Autodiscover>"
        );
        match PoxOutcome::parse(body.as_bytes()).unwrap() {
            PoxOutcome::RedirectAddr(a) => assert_eq!(a, "bob@other.com"),
            _ => panic!("expected RedirectAddr"),
        }
    }
}
