/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::BufRead;

use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};

use crate::exchange::ews::error::EwsError;
use crate::exchange::ews::parse::{Attrs, Ns, Trim};
use crate::exchange::ews::types::{ResponseCode, ServerVersion};
use crate::net::xml::Entity;

#[derive(Debug, Clone, Default)]
pub struct ServerVersionInfo {
    pub major_version: Option<u32>,
    pub minor_version: Option<u32>,
    pub major_build: Option<u32>,
    pub minor_build: Option<u32>,
    pub version: Option<String>,
}

#[derive(Clone, Copy)]
enum VersionAttr {
    MajorVersion,
    MinorVersion,
    MajorBuildNumber,
    MinorBuildNumber,
    Version,
}

impl ServerVersionInfo {
    pub fn to_server_version(&self) -> Option<ServerVersion> {
        match (self.major_version, self.minor_version) {
            (Some(maj), Some(min)) => Some(ServerVersion::from_build(maj, min)),
            _ => None,
        }
    }

    fn read_attrs(&mut self, e: &BytesStart<'_>) {
        for a in e.attributes().flatten() {
            let Some(attr) = hashify::map_ignore_case!(
                a.key.local_name().into_inner().as_bytes(),
                VersionAttr,
                "MajorVersion" => VersionAttr::MajorVersion,
                "MinorVersion" => VersionAttr::MinorVersion,
                "MajorBuildNumber" => VersionAttr::MajorBuildNumber,
                "MinorBuildNumber" => VersionAttr::MinorBuildNumber,
                "Version" => VersionAttr::Version,
            ) else {
                continue;
            };
            let value = &*a.value;
            match attr {
                VersionAttr::MajorVersion => self.major_version = value.parse().ok(),
                VersionAttr::MinorVersion => self.minor_version = value.parse().ok(),
                VersionAttr::MajorBuildNumber => self.major_build = value.parse().ok(),
                VersionAttr::MinorBuildNumber => self.minor_build = value.parse().ok(),
                VersionAttr::Version => self.version = Some(value.to_owned()),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SoapFault {
    pub fault_string: String,
    pub response_code: ResponseCode,
    pub back_off_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum EnvelopeKind {
    Fault {
        version: ServerVersionInfo,
        fault: SoapFault,
    },
    Body {
        version: ServerVersionInfo,
    },
}

#[derive(Clone, Copy)]
enum EnvelopeElement {
    ServerVersionInfo,
    Fault,
    Body,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FaultElement {
    FaultString,
    ResponseCode,
    Value,
}

impl EnvelopeKind {
    pub fn read(bytes: &[u8]) -> Result<EnvelopeKind, EwsError> {
        let mut xml = Trim::Text.reader(bytes);
        let mut buf = Vec::new();
        let mut version = ServerVersionInfo::default();
        let mut seen_body = false;
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    match hashify::map_ignore_case!(
                        e.local_name().into_inner().as_bytes(),
                        EnvelopeElement,
                        "ServerVersionInfo" => EnvelopeElement::ServerVersionInfo,
                        "Fault" => EnvelopeElement::Fault,
                        "Body" => EnvelopeElement::Body,
                    ) {
                        Some(EnvelopeElement::ServerVersionInfo) if Ns::of(&ns) == Ns::Types => {
                            version.read_attrs(e);
                        }
                        Some(EnvelopeElement::Fault) => {
                            let fault = SoapFault::read(&mut xml)?;
                            return Ok(EnvelopeKind::Fault { version, fault });
                        }
                        Some(EnvelopeElement::Body) => seen_body = true,
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if seen_body {
            Ok(EnvelopeKind::Body { version })
        } else {
            Err(EwsError::Malformed("envelope had no soap:Body".to_owned()))
        }
    }
}

impl SoapFault {
    fn read<R: BufRead>(xml: &mut NsReader<R>) -> Result<SoapFault, EwsError> {
        let mut buf = Vec::new();
        let mut fault_string = String::new();
        let mut code = ResponseCode::Other(String::new());
        let mut back_off: Option<u64> = None;
        let mut text_target: Option<FaultElement> = None;
        let mut last_value_name: Option<String> = None;
        let mut depth: u32 = 1;
        loop {
            buf.clear();
            let (ns, ev) = xml.read_resolved_event_into(&mut buf)?;
            match ev {
                Event::Start(e) => {
                    depth += 1;
                    text_target = match FaultElement::parse(e.local_name().into_inner()) {
                        Some(FaultElement::Value) if Ns::of(&ns) == Ns::Types => {
                            last_value_name = Attrs(&e).value("Name");
                            Some(FaultElement::Value)
                        }
                        Some(FaultElement::Value) | None => None,
                        other => other,
                    };
                }
                Event::Empty(e) => {
                    if Ns::of(&ns) == Ns::Types
                        && FaultElement::parse(e.local_name().into_inner())
                            == Some(FaultElement::Value)
                    {
                        last_value_name = Attrs(&e).value("Name");
                    }
                }
                Event::End(_) => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    text_target = None;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Text(t) => match text_target {
                    Some(FaultElement::FaultString) => fault_string.push_str(&t),
                    Some(FaultElement::ResponseCode) => code = ResponseCode::parse(&t),
                    Some(FaultElement::Value)
                        if last_value_name.as_deref() == Some("BackOffMilliseconds") =>
                    {
                        back_off = t.trim().parse().ok();
                    }
                    _ => {}
                },
                Event::CData(c) if text_target == Some(FaultElement::FaultString) => {
                    fault_string.push_str(&c);
                }
                Event::GeneralRef(ref g) if text_target == Some(FaultElement::FaultString) => {
                    if let Some(c) = Entity::from_ref(g) {
                        fault_string.push(c);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if matches!(code, ResponseCode::ServerBusy { .. }) {
            code = ResponseCode::ServerBusy {
                back_off_ms: back_off,
            };
        }
        Ok(SoapFault {
            fault_string,
            response_code: code,
            back_off_ms: back_off,
        })
    }
}

impl FaultElement {
    fn parse(local: &str) -> Option<FaultElement> {
        hashify::map_ignore_case!(
            local.as_bytes(),
            FaultElement,
            "faultstring" => FaultElement::FaultString,
            "ResponseCode" => FaultElement::ResponseCode,
            "Value" => FaultElement::Value,
        )
        .copied()
    }
}
