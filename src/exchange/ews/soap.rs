/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::fmt::Write;

use crate::exchange::ews::types::ServerVersion;
use crate::exchange::ews::xml::EwsRequest;
use crate::net::xml::XmlEscape;

const ENVELOPE_CAPACITY: usize = 512;

pub const NS_SOAP: &str = "http://schemas.xmlsoap.org/soap/envelope/";
pub const NS_TYPES: &str = "http://schemas.microsoft.com/exchange/services/2006/types";
pub const NS_MESSAGES: &str = "http://schemas.microsoft.com/exchange/services/2006/messages";

pub const SOAP_ACTION_BASE: &str = "http://schemas.microsoft.com/exchange/services/2006/messages";

#[derive(Debug, Clone, Copy)]
pub struct EnvelopeOptions<'a> {
    pub version: ServerVersion,
    pub impersonated_smtp: Option<&'a str>,
}

impl Default for EnvelopeOptions<'_> {
    fn default() -> Self {
        EnvelopeOptions {
            version: ServerVersion::Exchange2013Sp1,
            impersonated_smtp: None,
        }
    }
}

impl EnvelopeOptions<'_> {
    pub fn wrap(&self, request: &EwsRequest<'_>) -> String {
        let mut out = String::with_capacity(request.size_hint() + ENVELOPE_CAPACITY);
        let _ = write!(
            out,
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><soap:Envelope xmlns:soap=\"{NS_SOAP}\" xmlns:t=\"{NS_TYPES}\" xmlns:m=\"{NS_MESSAGES}\"><soap:Header><t:RequestServerVersion Version=\"{}\"/>",
            self.version.as_str()
        );
        if let Some(smtp) = self.impersonated_smtp {
            let _ = write!(
                out,
                "<t:ExchangeImpersonation><t:ConnectingSID><t:PrimarySmtpAddress>{}</t:PrimarySmtpAddress></t:ConnectingSID></t:ExchangeImpersonation>",
                XmlEscape(smtp)
            );
        }
        out.push_str("</soap:Header><soap:Body>");
        request.write_into(&mut out);
        out.push_str("</soap:Body></soap:Envelope>");
        out
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SoapAction<'a>(pub &'a str);

impl fmt::Display for SoapAction<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{SOAP_ACTION_BASE}/{}\"", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::ews::types::DistinguishedFolderId;
    use crate::exchange::ews::xml::{FolderRef, Traversal};

    const FIND_ROOT: EwsRequest<'static> = EwsRequest::FindFolder {
        parent: FolderRef::Distinguished(DistinguishedFolderId::MsgFolderRoot),
        traversal: Traversal::Shallow,
    };

    #[test]
    fn envelope_has_canonical_namespaces() {
        let env = EnvelopeOptions::default().wrap(&FIND_ROOT);
        assert!(env.contains("xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\""));
        assert!(
            env.contains("xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\"")
        );
        assert!(
            env.contains(
                "xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\""
            )
        );
        assert!(env.contains("<t:RequestServerVersion Version=\"Exchange2013_SP1\"/>"));
        assert!(env.contains("<soap:Body><m:FindFolder Traversal=\"Shallow\">"));
    }

    #[test]
    fn envelope_impersonation_includes_smtp() {
        let opts = EnvelopeOptions {
            version: ServerVersion::Exchange2016,
            impersonated_smtp: Some("alice@contoso.com"),
        };
        let env = opts.wrap(&FIND_ROOT);
        assert!(env.contains("<t:ExchangeImpersonation>"));
        assert!(env.contains("<t:PrimarySmtpAddress>alice@contoso.com</t:PrimarySmtpAddress>"));
        assert!(env.contains("Version=\"Exchange2016\""));
    }

    #[test]
    fn soap_action_quotes_operation() {
        assert_eq!(
            SoapAction("FindItem").to_string(),
            "\"http://schemas.microsoft.com/exchange/services/2006/messages/FindItem\""
        );
    }
}
