/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use quick_xml::NsReader;
use quick_xml::events::Event;
use serde_json::Value;
use ureq::Agent;
use ureq::config::Config;
use ureq::tls::{RootCerts, TlsConfig};

use crate::exchange_ews::error::EwsError;
use crate::exchange_ews::parse::entity_to_char;

const V2_HOST: &str = "https://outlook.office365.com";
const POX_REQ_NS: &str =
    "http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006";
const POX_RESP_NS: &str =
    "http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a";

pub fn is_fully_qualified_ews_url(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    if parsed.scheme() != "https" {
        return false;
    }
    parsed.path().eq_ignore_ascii_case("/EWS/Exchange.asmx")
}

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

pub fn discover(
    supplied_url: Option<&str>,
    email: Option<&str>,
    auth_header: Option<&str>,
    allow_invalid_certs: bool,
) -> Result<DiscoveryResult, EwsError> {
    if let Some(url) = supplied_url
        && is_fully_qualified_ews_url(url)
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
    let agent = build_agent(allow_invalid_certs);
    if let Ok(url) = autodiscover_v2(&agent, email) {
        return Ok(DiscoveryResult {
            ews_url: url,
            source: DiscoverySource::V2,
        });
    }
    let mut tried: Vec<String> = Vec::new();
    let mut redirects: u8 = 0;
    let mut url_redirects: u8 = 0;
    let mut current_email = email.to_owned();
    'outer: loop {
        let domain = current_email
            .split('@')
            .nth(1)
            .ok_or_else(|| EwsError::AutodiscoverFailed("--mailbox is not an email".to_owned()))?;
        let candidates = pox_candidates(domain);
        for candidate in &candidates {
            tried.push(candidate.clone());
            match autodiscover_v1(&agent, candidate, &current_email, auth_header) {
                Ok(PoxOutcome::EwsUrl(url)) => {
                    return Ok(DiscoveryResult {
                        ews_url: url,
                        source: DiscoverySource::V1,
                    });
                }
                Ok(PoxOutcome::RedirectAddr(addr)) => {
                    if redirects >= 4 {
                        return Err(EwsError::AutodiscoverLoop);
                    }
                    redirects += 1;
                    current_email = addr;
                    continue 'outer;
                }
                Ok(PoxOutcome::RedirectUrl(url)) => {
                    if url_redirects >= 1 {
                        return Err(EwsError::AutodiscoverLoop);
                    }
                    url_redirects += 1;
                    tried.push(url.clone());
                    if let Ok(PoxOutcome::EwsUrl(u)) =
                        autodiscover_v1(&agent, &url, &current_email, auth_header)
                    {
                        return Ok(DiscoveryResult {
                            ews_url: u,
                            source: DiscoverySource::V1,
                        });
                    }
                    continue;
                }
                Err(_) | Ok(PoxOutcome::Failed) => continue,
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
        "tried {} candidate(s), none returned a v1 EwsUrl",
        tried.len()
    )))
}

fn build_agent(allow_invalid_certs: bool) -> Agent {
    let config: Config = Config::builder()
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                .unversioned_rustls_crypto_provider(std::sync::Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
                ))
                .root_certs(RootCerts::PlatformVerifier)
                .disable_verification(allow_invalid_certs)
                .build(),
        )
        .build();
    config.new_agent()
}

fn autodiscover_v2(agent: &Agent, email: &str) -> Result<String, EwsError> {
    let url = format!(
        "{V2_HOST}/autodiscover/autodiscover.json?Email={}&Protocol=Ews",
        urlencode(email)
    );
    let mut resp = agent
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
    let protocol = body.get("Protocol").and_then(Value::as_str).unwrap_or("");
    if !protocol.eq_ignore_ascii_case("Ews") {
        return Err(EwsError::AutodiscoverFailed(format!(
            "v2 returned protocol {protocol:?}"
        )));
    }
    let url = body
        .get("Url")
        .and_then(Value::as_str)
        .ok_or_else(|| EwsError::AutodiscoverFailed("v2 missing Url".to_owned()))?;
    Ok(url.to_owned())
}

#[derive(Debug, Clone)]
enum PoxOutcome {
    EwsUrl(String),
    RedirectAddr(String),
    RedirectUrl(String),
    Failed,
}

fn pox_candidates(domain: &str) -> Vec<String> {
    let mut out = vec![
        format!("https://autodiscover.{domain}/autodiscover/autodiscover.xml"),
        format!("https://{domain}/autodiscover/autodiscover.xml"),
    ];
    for SrvRecord { target, port, .. } in srv_lookup(&format!("_autodiscover._tcp.{domain}")) {
        let host = target.trim_end_matches('.');
        let suffix = if port == 443 {
            String::new()
        } else {
            format!(":{port}")
        };
        out.push(format!(
            "https://{host}{suffix}/autodiscover/autodiscover.xml"
        ));
    }
    out
}

#[derive(Debug, Clone)]
struct SrvRecord {
    priority: u16,
    weight: u16,
    port: u16,
    target: String,
}

fn srv_lookup(qname: &str) -> Vec<SrvRecord> {
    use std::net::UdpSocket;
    use std::time::Duration;

    let nameservers = resolv_conf_nameservers();
    if nameservers.is_empty() {
        return Vec::new();
    }
    let mut query = Vec::with_capacity(64);
    let txn_id: u16 = 0x1234;
    query.extend_from_slice(&txn_id.to_be_bytes());
    query.extend_from_slice(&0x0100u16.to_be_bytes());
    query.extend_from_slice(&1u16.to_be_bytes());
    query.extend_from_slice(&[0u8; 6]);
    encode_qname(&mut query, qname);
    query.extend_from_slice(&33u16.to_be_bytes());
    query.extend_from_slice(&1u16.to_be_bytes());

    let mut out: Vec<SrvRecord> = Vec::new();
    for ns in nameservers {
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        let _ = socket.set_read_timeout(Some(Duration::from_secs(3)));
        if socket.send_to(&query, format!("{ns}:53")).is_err() {
            continue;
        }
        let mut buf = [0u8; 1500];
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            continue;
        };
        if let Some(records) = parse_srv_response(&buf[..n]) {
            out = records;
            break;
        }
    }
    out.sort_by(|a, b| a.priority.cmp(&b.priority).then(b.weight.cmp(&a.weight)));
    out
}

fn resolv_conf_nameservers() -> Vec<String> {
    let path = std::path::Path::new("/etc/resolv.conf");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("nameserver") {
            let ns = rest.trim();
            if !ns.is_empty() {
                out.push(ns.to_owned());
            }
        }
    }
    out
}

fn encode_qname(out: &mut Vec<u8>, qname: &str) {
    for label in qname.trim_end_matches('.').split('.') {
        if label.is_empty() {
            continue;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn parse_srv_response(buf: &[u8]) -> Option<Vec<SrvRecord>> {
    if buf.len() < 12 {
        return None;
    }
    let qd = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let an = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let mut cursor = 12;
    for _ in 0..qd {
        cursor = skip_name(buf, cursor)?;
        cursor = cursor.checked_add(4)?;
        if cursor > buf.len() {
            return None;
        }
    }
    let mut out: Vec<SrvRecord> = Vec::new();
    for _ in 0..an {
        cursor = skip_name(buf, cursor)?;
        if cursor + 10 > buf.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([buf[cursor], buf[cursor + 1]]);
        let rdlength = u16::from_be_bytes([buf[cursor + 8], buf[cursor + 9]]) as usize;
        cursor += 10;
        let rdata_end = cursor.checked_add(rdlength)?;
        if rdata_end > buf.len() {
            return None;
        }
        if rtype == 33 && rdlength >= 7 {
            let priority = u16::from_be_bytes([buf[cursor], buf[cursor + 1]]);
            let weight = u16::from_be_bytes([buf[cursor + 2], buf[cursor + 3]]);
            let port = u16::from_be_bytes([buf[cursor + 4], buf[cursor + 5]]);
            let target = read_name(buf, cursor + 6)?;
            out.push(SrvRecord {
                priority,
                weight,
                port,
                target,
            });
        }
        cursor = rdata_end;
    }
    Some(out)
}

fn skip_name(buf: &[u8], mut cursor: usize) -> Option<usize> {
    loop {
        if cursor >= buf.len() {
            return None;
        }
        let len = buf[cursor];
        if len == 0 {
            return Some(cursor + 1);
        }
        if (len & 0xC0) == 0xC0 {
            return Some(cursor + 2);
        }
        cursor = cursor.checked_add(1 + len as usize)?;
    }
}

fn read_name(buf: &[u8], start: usize) -> Option<String> {
    let mut out = String::new();
    let mut cursor = start;
    let mut hops = 0;
    loop {
        if cursor >= buf.len() {
            return None;
        }
        let len = buf[cursor];
        if len == 0 {
            break;
        }
        if (len & 0xC0) == 0xC0 {
            if cursor + 1 >= buf.len() {
                return None;
            }
            let pointer = ((len & 0x3F) as usize) << 8 | (buf[cursor + 1] as usize);
            hops += 1;
            if hops > 8 {
                return None;
            }
            cursor = pointer;
            continue;
        }
        cursor += 1;
        let end = cursor.checked_add(len as usize)?;
        if end > buf.len() {
            return None;
        }
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(std::str::from_utf8(&buf[cursor..end]).ok()?);
        cursor = end;
    }
    Some(out)
}

fn autodiscover_v1(
    agent: &Agent,
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
        xml_escape(email)
    );
    let mut req = agent
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
        .limit(2 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| EwsError::Malformed(format!("v1 body: {e}")))?;
    parse_pox_response(&bytes)
}

fn parse_pox_response(body: &[u8]) -> Result<PoxOutcome, EwsError> {
    let mut xml = NsReader::from_reader(body);
    xml.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current: Option<&'static str> = None;
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
                let local = e.local_name().as_ref().to_owned();
                cur.clear();
                if local.eq_ignore_ascii_case("Protocol") {
                    current_type = None;
                }
                if local.eq_ignore_ascii_case("Type") {
                    current = Some("type");
                } else if local.eq_ignore_ascii_case("EwsUrl") {
                    if matches!(current_type.as_deref(), Some("EXPR") | Some("EXCH")) {
                        current = Some("ewsUrl");
                    } else {
                        current = None;
                    }
                } else if local.eq_ignore_ascii_case("Action") {
                    current = Some("action");
                } else if local.eq_ignore_ascii_case("RedirectAddr") {
                    current = Some("redirectAddr");
                } else if local.eq_ignore_ascii_case("RedirectUrl") {
                    current = Some("redirectUrl");
                } else {
                    current = None;
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_owned();
                if let Some(field) = current.take() {
                    let text = std::mem::take(&mut cur);
                    match field {
                        "type" => current_type = Some(text),
                        "ewsUrl" => ews_url = Some(text),
                        "action" => action = Some(text),
                        "redirectAddr" => redirect_addr = Some(text),
                        "redirectUrl" => redirect_url = Some(text),
                        _ => {}
                    }
                }
                cur.clear();
                if local.eq_ignore_ascii_case("Protocol") {
                    current_type = None;
                }
            }
            Event::Text(t) => {
                cur.push_str(&t);
            }
            Event::CData(c) => {
                cur.push_str(&c);
            }
            Event::GeneralRef(g) => {
                if let Some(c) = entity_to_char(&g) {
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
    if let Some(url) = ews_url {
        return Ok(PoxOutcome::EwsUrl(url));
    }
    Ok(PoxOutcome::Failed)
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_fully_qualified_ews_url() {
        assert!(is_fully_qualified_ews_url(
            "https://outlook.office365.com/EWS/Exchange.asmx"
        ));
        assert!(is_fully_qualified_ews_url(
            "https://exchange.example.com/EWS/Exchange.asmx"
        ));
        assert!(!is_fully_qualified_ews_url(
            "http://outlook.office365.com/EWS/Exchange.asmx"
        ));
        assert!(!is_fully_qualified_ews_url(
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
        match parse_pox_response(body.as_bytes()).unwrap() {
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
        match parse_pox_response(body.as_bytes()).unwrap() {
            PoxOutcome::RedirectAddr(a) => assert_eq!(a, "bob@other.com"),
            _ => panic!("expected RedirectAddr"),
        }
    }
}
