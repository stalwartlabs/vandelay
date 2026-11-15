/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{BufReader, Write};
use std::time::Instant;

use encodify::base64::STANDARD;

use super::command::{Command, Tags};
use super::error::ImapError;
use super::response::{Response, StatusLine, Untagged};
use crate::logging::{Level, Logger};
use crate::net::sasl::SaslPayload;
use crate::net::stream::{ConnectMode, Connector, Stream};

const CRLF: &[u8] = b"\r\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    StartTls,
    LoginDisabled,
    SaslIr,
    Enable,
    Utf8Accept,
    CompressDeflate,
    LiteralPlus,
    LiteralMinus,
    AuthPlain,
    AuthXoauth2,
    AuthOauthBearer,
    ListExtended,
    ListStatus,
    SpecialUse,
    Namespace,
    Esearch,
}

impl Capability {
    pub const fn name(self) -> &'static str {
        match self {
            Capability::StartTls => "STARTTLS",
            Capability::LoginDisabled => "LOGINDISABLED",
            Capability::SaslIr => "SASL-IR",
            Capability::Enable => "ENABLE",
            Capability::Utf8Accept => "UTF8=ACCEPT",
            Capability::CompressDeflate => "COMPRESS=DEFLATE",
            Capability::LiteralPlus => "LITERAL+",
            Capability::LiteralMinus => "LITERAL-",
            Capability::AuthPlain => "AUTH=PLAIN",
            Capability::AuthXoauth2 => "AUTH=XOAUTH2",
            Capability::AuthOauthBearer => "AUTH=OAUTHBEARER",
            Capability::ListExtended => "LIST-EXTENDED",
            Capability::ListStatus => "LIST-STATUS",
            Capability::SpecialUse => "SPECIAL-USE",
            Capability::Namespace => "NAMESPACE",
            Capability::Esearch => "ESEARCH",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Capabilities(BTreeSet<Box<str>>);

impl Capabilities {
    pub fn has(&self, capability: Capability) -> bool {
        self.0.contains(capability.name())
    }

    pub fn contains(&self, token: &str) -> bool {
        self.0.contains(token.to_ascii_uppercase().as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn replace<'t>(&mut self, tokens: impl IntoIterator<Item = &'t str>) {
        self.0.clear();
        self.insert(tokens);
    }

    pub fn insert<'t>(&mut self, tokens: impl IntoIterator<Item = &'t str>) {
        self.0
            .extend(tokens.into_iter().map(|t| t.to_ascii_uppercase().into()));
    }
}

#[derive(Debug)]
pub struct CollectedResponse {
    pub line: StatusLine,
    pub untagged: Vec<Untagged>,
}

pub struct ImapClient {
    reader: BufReader<Stream>,
    tags: Tags,
    capabilities: Capabilities,
    host: String,
    closed: bool,
    utf8_accept: bool,
    logger: Logger,
    line: String,
}

impl ImapClient {
    pub fn connect(
        connector: &Connector,
        host: &str,
        port: u16,
        mode: ConnectMode,
        logger: Logger,
    ) -> Result<ImapClient, ImapError> {
        let stream = match mode {
            ConnectMode::ImplicitTls => connector.connect_tls(host, port)?,
            ConnectMode::Plain | ConnectMode::StartTls => connector.connect_plain(host, port)?,
        };
        let mut client = ImapClient::over(stream, host, logger);
        client.read_greeting()?;
        client.refresh_capabilities()?;
        if mode == ConnectMode::StartTls {
            if !client.has(Capability::StartTls) {
                return Err(ImapError::Unsupported(
                    "server does not advertise STARTTLS but cleartext credentials are forbidden"
                        .into(),
                ));
            }
            client.starttls(connector)?;
            client.refresh_capabilities()?;
        }
        Ok(client)
    }

    fn over(stream: Stream, host: &str, logger: Logger) -> ImapClient {
        ImapClient {
            reader: BufReader::new(stream),
            tags: Tags::default(),
            capabilities: Capabilities::default(),
            host: host.to_owned(),
            closed: false,
            utf8_accept: false,
            logger,
            line: String::new(),
        }
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.has(capability)
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn utf8_accept(&self) -> bool {
        self.utf8_accept
    }

    fn read_greeting(&mut self) -> Result<(), ImapError> {
        match Response::read(&mut self.reader)? {
            Response::Untagged(Untagged::StatusLine(line)) => {
                self.absorb_code(&line);
                Ok(())
            }
            Response::Untagged(Untagged::Bye(text)) => Err(ImapError::Bye(text)),
            other => Err(ImapError::Protocol(format!(
                "expected greeting, got {other:?}"
            ))),
        }
    }

    pub fn refresh_capabilities(&mut self) -> Result<(), ImapError> {
        self.run_collect(&Command::Capability)?;
        if self.capabilities.is_empty() {
            return Err(ImapError::Protocol(
                "server returned no CAPABILITY response".into(),
            ));
        }
        Ok(())
    }

    fn starttls(&mut self, connector: &Connector) -> Result<(), ImapError> {
        self.run_collect(&Command::StartTls)?;
        if self.reader.get_ref().is_tls() {
            return Err(ImapError::Protocol(
                "STARTTLS issued but stream already TLS".into(),
            ));
        }
        let stream = self.reader.get_mut().take();
        self.reader = BufReader::new(stream.upgrade_tls(connector, &self.host)?);
        Ok(())
    }

    pub fn enable(&mut self, extensions: &[&str]) -> Result<Vec<String>, ImapError> {
        if !self.has(Capability::Enable) {
            return Err(ImapError::Unsupported(
                "server does not advertise ENABLE".into(),
            ));
        }
        let mut enabled = Vec::new();
        for untagged in self.run_collect(&Command::Enable(extensions))?.untagged {
            if let Untagged::Enabled(exts) = untagged {
                enabled.extend(exts);
            }
        }
        if enabled
            .iter()
            .any(|e| e.eq_ignore_ascii_case(Capability::Utf8Accept.name()))
        {
            self.utf8_accept = true;
        }
        Ok(enabled)
    }

    pub fn compress_deflate(&mut self) -> Result<(), ImapError> {
        if !self.has(Capability::CompressDeflate) {
            return Err(ImapError::Unsupported(
                "server does not advertise COMPRESS=DEFLATE".into(),
            ));
        }
        self.run_collect(&Command::CompressDeflate)?;
        let primer = self.reader.buffer().to_vec();
        let stream = self.reader.get_mut().take();
        self.reader = BufReader::new(stream.compress(&primer));
        Ok(())
    }

    pub fn login(&mut self, user: &str, password: &str) -> Result<(), ImapError> {
        if self.has(Capability::LoginDisabled) {
            return Err(ImapError::Unsupported(
                "server advertises LOGINDISABLED".into(),
            ));
        }
        self.run_collect(&Command::Login { user, password })
            .map(drop)
            .map_err(ImapError::into_auth_failure)
    }

    pub fn authenticate(&mut self, payload: SaslPayload<'_>) -> Result<(), ImapError> {
        if self.has(Capability::SaslIr) {
            return self
                .run_collect(&Command::AuthenticateIr(payload))
                .map(drop)
                .map_err(ImapError::into_auth_failure);
        }
        let tag = self.send(&Command::Authenticate(payload.mechanism()))?;
        let response = payload.bytes();
        let mut sent = false;
        loop {
            match Response::read(&mut self.reader)? {
                Response::Continuation(_) => {
                    if sent && !matches!(payload, SaslPayload::Plain { .. }) {
                        self.write_all(CRLF)?;
                    } else {
                        let mut line =
                            Vec::with_capacity(STANDARD.encoded_len(response.len()) + CRLF.len());
                        STANDARD.encode_append(&response, &mut line);
                        line.extend_from_slice(CRLF);
                        self.write_all(&line)?;
                        sent = true;
                    }
                }
                Response::Tagged { tag: t, line } if t == tag => {
                    self.absorb_code(&line);
                    return line
                        .into_result()
                        .map(drop)
                        .map_err(ImapError::into_auth_failure);
                }
                Response::Untagged(Untagged::Capability(caps)) => {
                    self.capabilities.replace(caps.iter().map(String::as_str));
                }
                _ => {}
            }
        }
    }

    pub fn noop(&mut self) -> Result<CollectedResponse, ImapError> {
        self.run_collect(&Command::Noop)
    }

    pub fn logout(&mut self) -> Result<(), ImapError> {
        if self.closed {
            return Ok(());
        }
        let _ = self.run_collect(&Command::Logout);
        self.closed = true;
        Ok(())
    }

    pub fn run_streamed(
        &mut self,
        command: &Command<'_>,
        mut on_untagged: impl FnMut(Untagged),
    ) -> Result<StatusLine, ImapError> {
        let tag = self.send(command)?;
        let started = Instant::now();
        loop {
            match Response::read(&mut self.reader)? {
                Response::Tagged { tag: t, line } if t == tag => {
                    self.absorb_code(&line);
                    if self.logger.enabled(Level::Method) {
                        let status = format!("{} {}", line.status, line.text);
                        self.logger
                            .trace_cmd("IMAP", &self.line, &status, started.elapsed());
                    }
                    return line.into_result();
                }
                Response::Tagged { tag: other, .. } => {
                    return Err(ImapError::Protocol(format!(
                        "expected tag {tag}, got {other}"
                    )));
                }
                Response::Untagged(Untagged::Bye(text)) => {
                    self.closed = true;
                    return Err(ImapError::Bye(text));
                }
                Response::Untagged(untagged) => {
                    if let Untagged::Capability(caps) = &untagged {
                        self.capabilities.replace(caps.iter().map(String::as_str));
                    }
                    on_untagged(untagged);
                }
                Response::Continuation(_) => {
                    return Err(ImapError::Protocol(
                        "unexpected continuation outside AUTHENTICATE".into(),
                    ));
                }
            }
        }
    }

    pub fn run_collect(&mut self, command: &Command<'_>) -> Result<CollectedResponse, ImapError> {
        let mut untagged = Vec::new();
        let line = self.run_streamed(command, |u| untagged.push(u))?;
        Ok(CollectedResponse { line, untagged })
    }

    fn send(&mut self, command: &Command<'_>) -> Result<String, ImapError> {
        self.line.clear();
        let _ = write!(self.line, "{command}");
        let tag = self.tags.issue();
        if Command::has_literal(&self.line)
            && !self.has(Capability::LiteralPlus)
            && !self.has(Capability::LiteralMinus)
        {
            return Err(ImapError::Unsupported(
                "command contains a non-ASCII literal but server does not advertise LITERAL+ or LITERAL-"
                    .into(),
            ));
        }
        let stream = self.reader.get_mut();
        stream.write_all(tag.as_bytes())?;
        stream.write_all(b" ")?;
        stream.write_all(self.line.as_bytes())?;
        stream.write_all(CRLF)?;
        stream.flush()?;
        Ok(tag)
    }

    fn absorb_code(&mut self, line: &StatusLine) {
        if line.code.as_deref() == Some("CAPABILITY")
            && let Some(args) = &line.code_args
        {
            self.capabilities.replace(args.split_ascii_whitespace());
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), ImapError> {
        let stream = self.reader.get_mut();
        stream.write_all(bytes)?;
        stream.flush()?;
        Ok(())
    }
}

impl Drop for ImapClient {
    fn drop(&mut self) {
        let _ = self.logout();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap::response::Status;
    use crate::net::stream::Stream;

    fn client_with(server: &[u8]) -> ImapClient {
        ImapClient::over(
            Stream::mock(server),
            "test.example",
            Logger::new(Level::Quiet),
        )
    }

    fn written(client: &ImapClient) -> String {
        String::from_utf8(client.reader.get_ref().written().to_vec()).unwrap()
    }

    fn plain() -> SaslPayload<'static> {
        SaslPayload::Plain {
            authcid: "alice",
            password: "p@ss",
        }
    }

    #[test]
    fn run_collect_drains_untagged_and_returns_ok() {
        let server = b"* CAPABILITY IMAP4rev1 STARTTLS\r\nA0001 OK done\r\n";
        let mut c = client_with(server);
        let r = c.run_collect(&Command::Capability).unwrap();
        assert_eq!(r.untagged.len(), 1);
        assert!(matches!(r.line.status, Status::Ok));
        assert!(c.has(Capability::StartTls));
        assert_eq!(written(&c), "A0001 CAPABILITY\r\n");
    }

    #[test]
    fn commands_go_out_tagged_and_crlf_terminated() {
        let mut c = client_with(b"A0001 OK done\r\n");
        c.noop().unwrap();
        assert_eq!(written(&c), "A0001 NOOP\r\n");
    }

    #[test]
    fn run_collect_propagates_no_as_imaperror_no() {
        let mut c = client_with(b"A0001 NO mailbox not found\r\n");
        match c.run_collect(&Command::Select("bogus")).unwrap_err() {
            ImapError::No(no) => assert_eq!(no.text, "mailbox not found"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn run_collect_propagates_bad_as_imaperror_bad() {
        let mut c = client_with(b"A0001 BAD syntax error\r\n");
        let err = c.run_collect(&Command::Raw("BOGUS")).unwrap_err();
        assert!(matches!(err, ImapError::Bad(_)));
    }

    #[test]
    fn login_translates_authenticationfailed() {
        let mut c = client_with(b"A0001 NO [AUTHENTICATIONFAILED] bad creds\r\n");
        let err = c.login("alice", "wrong").unwrap_err();
        assert!(matches!(err, ImapError::AuthFailed(_)));
    }

    #[test]
    fn login_refused_when_logindisabled() {
        let mut c = client_with(b"A0001 OK any\r\n");
        c.capabilities.insert(["LOGINDISABLED"]);
        let err = c.login("a", "b").unwrap_err();
        assert!(matches!(err, ImapError::Unsupported(_)));
    }

    #[test]
    fn bye_mid_response_closes_client() {
        let mut c = client_with(b"* BYE going away\r\n");
        let err = c.run_collect(&Command::Noop).unwrap_err();
        assert!(matches!(err, ImapError::Bye(_)));
        assert!(c.closed);
    }

    #[test]
    fn authenticate_plain_sasl_ir_path() {
        let mut c = client_with(b"A0001 OK auth done\r\n");
        c.capabilities.insert(["SASL-IR"]);
        c.authenticate(plain()).unwrap();
        assert_eq!(written(&c), "A0001 AUTHENTICATE PLAIN AGFsaWNlAHBAc3M=\r\n");
    }

    #[test]
    fn authenticate_plain_accepts_dovecot_post_login_untagged_capability() {
        let server = b"* CAPABILITY IMAP4rev1 SASL-IR MOVE IMAPSIEVE=sieve://127.0.0.1:4190 QUOTA\r\nA0001 OK [CAPABILITY IMAP4rev1 SASL-IR MOVE IMAPSIEVE=sieve://127.0.0.1:4190 QUOTA] Logged in\r\n";
        let mut c = client_with(server);
        c.capabilities.insert(["SASL-IR"]);
        c.authenticate(plain()).unwrap();
        assert!(c.capabilities().contains("MOVE"));
        assert!(
            c.capabilities()
                .contains("IMAPSIEVE=sieve://127.0.0.1:4190")
        );
    }

    #[test]
    fn authenticate_plain_continuation_path() {
        let mut c = client_with(b"+ \r\nA0001 OK auth done\r\n");
        c.authenticate(plain()).unwrap();
        assert_eq!(
            written(&c),
            "A0001 AUTHENTICATE PLAIN\r\nAGFsaWNlAHBAc3M=\r\n"
        );
    }

    #[test]
    fn authenticate_bearer_answers_an_error_challenge_with_an_empty_line() {
        let mut c = client_with(
            b"+ \r\n+ eyJzdGF0dXMiOiI0MDEifQ==\r\nA0001 NO [AUTHENTICATIONFAILED] no\r\n",
        );
        let err = c
            .authenticate(SaslPayload::XOauth2 {
                user: "u",
                token: "t",
            })
            .unwrap_err();
        assert!(matches!(err, ImapError::AuthFailed(_)));
        assert!(written(&c).ends_with("\r\n\r\n"));
    }

    #[test]
    fn capabilities_are_case_insensitive() {
        let mut c = client_with(b"");
        c.capabilities.insert(["starttls"]);
        assert!(c.has(Capability::StartTls));
        assert!(c.capabilities().contains("StartTLS"));
        assert!(!c.capabilities().contains("MOVE"));
    }

    #[test]
    fn literal_without_literal_plus_is_refused() {
        let mut c = client_with(b"");
        let err = c.run_collect(&Command::Select("éclair")).unwrap_err();
        assert!(matches!(err, ImapError::Unsupported(_)));
    }
}
