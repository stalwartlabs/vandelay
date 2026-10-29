/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::BTreeSet;
use std::io::{BufReader, Read, Write};
use std::time::Instant;

use super::command::{self, CommandBuilder};
use super::error::{ImapError, NoError};
use super::response::{Response, Status, StatusLine, Untagged, parse_response};
use super::transport::{Connector, ImapStream};
use crate::logging::Logger;

pub struct ImapClient {
    reader: BufReader<Box<dyn ImapStream>>,
    tags: CommandBuilder,
    pub capabilities: BTreeSet<String>,
    pub host: String,
    closed: bool,
    utf8_accept: bool,
    logger: Logger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMode {
    ImplicitTls,
    Plain,
    StartTls,
}

#[derive(Debug, Clone)]
pub struct Greeting {
    pub status: Status,
    pub text: String,
    pub code: Option<String>,
}

impl ImapClient {
    pub fn connect(
        connector: &Connector,
        host: &str,
        port: u16,
        mode: ConnectMode,
        logger: Logger,
    ) -> Result<ImapClient, ImapError> {
        let stream: Box<dyn ImapStream> = match mode {
            ConnectMode::ImplicitTls => connector.connect_tls(host, port)?,
            ConnectMode::Plain | ConnectMode::StartTls => connector.connect_plain(host, port)?,
        };
        let mut client = ImapClient {
            reader: BufReader::new(stream),
            tags: CommandBuilder::new(),
            capabilities: BTreeSet::new(),
            host: host.to_owned(),
            closed: false,
            utf8_accept: false,
            logger,
        };
        client.read_greeting()?;
        client.send_capability()?;
        if matches!(mode, ConnectMode::StartTls) {
            if !client.has_capability("STARTTLS") {
                return Err(ImapError::Unsupported(
                    "server does not advertise STARTTLS but cleartext credentials are forbidden"
                        .into(),
                ));
            }
            client.starttls(connector)?;
            client.send_capability()?;
        }
        Ok(client)
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
    }

    fn read_greeting(&mut self) -> Result<Greeting, ImapError> {
        let resp = parse_response(&mut self.reader)?;
        match resp {
            Response::Untagged(Untagged::StatusLine(line)) => {
                if let Some(code) = &line.code
                    && code == "CAPABILITY"
                    && let Some(args) = &line.code_args
                {
                    self.capabilities = parse_caps(args);
                }
                Ok(Greeting {
                    status: line.status,
                    text: line.text,
                    code: line.code,
                })
            }
            Response::Untagged(Untagged::Bye(text)) => Err(ImapError::Bye(text)),
            other => Err(ImapError::Protocol(format!(
                "expected greeting, got {other:?}"
            ))),
        }
    }

    pub fn refresh_capabilities(&mut self) -> Result<(), ImapError> {
        self.send_capability()
    }

    fn send_capability(&mut self) -> Result<(), ImapError> {
        let resp = self.run_collect(command::capability())?;
        for u in resp.untagged {
            if let Untagged::Capability(caps) = u {
                self.capabilities = caps.into_iter().collect();
            }
        }
        if self.capabilities.is_empty() {
            return Err(ImapError::Protocol(
                "server returned no CAPABILITY response".into(),
            ));
        }
        Ok(())
    }

    fn starttls(&mut self, connector: &Connector) -> Result<(), ImapError> {
        self.run_collect(command::starttls())?;
        let stream = self.reader.get_ref();
        if stream.is_tls() {
            return Err(ImapError::Protocol(
                "STARTTLS issued but stream already TLS".into(),
            ));
        }
        let buffered = std::mem::replace(&mut self.reader, BufReader::new(noop_stream()));
        let inner = buffered.into_inner();
        let upgraded = inner.upgrade_tls(connector, &self.host)?;
        self.reader = BufReader::new(upgraded);
        Ok(())
    }

    pub fn utf8_accept(&self) -> bool {
        self.utf8_accept
    }

    pub fn enable(&mut self, extensions: &[&str]) -> Result<Vec<String>, ImapError> {
        if !self.has_capability("ENABLE") {
            return Err(ImapError::Unsupported(
                "server does not advertise ENABLE".into(),
            ));
        }
        let resp = self.run_collect(&command::enable(extensions))?;
        let mut enabled: Vec<String> = Vec::new();
        for u in resp.untagged {
            if let Untagged::Enabled(exts) = u {
                enabled.extend(exts);
            }
        }
        if enabled
            .iter()
            .any(|e| e.eq_ignore_ascii_case("UTF8=ACCEPT"))
        {
            self.utf8_accept = true;
        }
        Ok(enabled)
    }

    pub fn compress_deflate(&mut self) -> Result<(), ImapError> {
        if !self.has_capability("COMPRESS=DEFLATE") {
            return Err(ImapError::Unsupported(
                "server does not advertise COMPRESS=DEFLATE".into(),
            ));
        }
        self.run_collect(command::compress_deflate())?;
        let buffered = std::mem::replace(&mut self.reader, BufReader::new(noop_stream()));
        let primer = buffered.buffer().to_vec();
        let inner = buffered.into_inner();
        let deflated = super::transport::DeflateImapStream::wrap(inner, &primer);
        self.reader = BufReader::new(deflated);
        Ok(())
    }

    pub fn login(&mut self, user: &str, password: &str) -> Result<(), ImapError> {
        if self.capabilities.iter().any(|c| c == "LOGINDISABLED") {
            return Err(ImapError::Unsupported(
                "server advertises LOGINDISABLED".into(),
            ));
        }
        let cmd = command::login(user, password);
        match self.run_collect(&cmd) {
            Ok(_) => Ok(()),
            Err(ImapError::No(no)) if no.is_auth_failed() => Err(ImapError::AuthFailed(no.text)),
            Err(e) => Err(e),
        }
    }

    pub fn authenticate_plain(&mut self, authcid: &str, password: &str) -> Result<(), ImapError> {
        let mut payload = Vec::with_capacity(authcid.len() + password.len() + 3);
        payload.push(0);
        payload.extend_from_slice(authcid.as_bytes());
        payload.push(0);
        payload.extend_from_slice(password.as_bytes());
        if self.has_capability("SASL-IR") {
            let cmd = command::authenticate_with_ir("PLAIN", &payload);
            return match self.run_collect(&cmd) {
                Ok(_) => Ok(()),
                Err(ImapError::No(no)) if no.is_auth_failed() => {
                    Err(ImapError::AuthFailed(no.text))
                }
                Err(e) => Err(e),
            };
        }
        let (tag, bytes) = self.tags.build(&command::authenticate("PLAIN"));
        self.write_all(&bytes)?;
        loop {
            let resp = parse_response(&mut self.reader)?;
            match resp {
                Response::Continuation(_) => {
                    self.write_all(&command::sasl_response(&payload))?;
                }
                Response::Tagged { tag: t, line } if t == tag => {
                    self.update_capabilities_from_code(&line);
                    return self.finish_tagged(line);
                }
                Response::Untagged(Untagged::Capability(caps)) => {
                    self.capabilities = caps.into_iter().collect();
                }
                _ => {}
            }
        }
    }

    pub fn authenticate_xoauth2(&mut self, user: &str, token: &str) -> Result<(), ImapError> {
        let mut payload = String::with_capacity(user.len() + token.len() + 32);
        payload.push_str("user=");
        payload.push_str(user);
        payload.push('\x01');
        payload.push_str("auth=Bearer ");
        payload.push_str(token);
        payload.push('\x01');
        payload.push('\x01');
        self.drive_bearer_sasl("XOAUTH2", payload.as_bytes())
    }

    pub fn authenticate_oauthbearer(&mut self, user: &str, token: &str) -> Result<(), ImapError> {
        let mut payload = String::with_capacity(user.len() + token.len() + 32);
        payload.push_str("n,a=");
        payload.push_str(user);
        payload.push(',');
        payload.push('\x01');
        payload.push_str("auth=Bearer ");
        payload.push_str(token);
        payload.push('\x01');
        payload.push('\x01');
        self.drive_bearer_sasl("OAUTHBEARER", payload.as_bytes())
    }

    fn drive_bearer_sasl(&mut self, mechanism: &str, payload: &[u8]) -> Result<(), ImapError> {
        if self.has_capability("SASL-IR") {
            let cmd = command::authenticate_with_ir(mechanism, payload);
            return match self.run_collect(&cmd) {
                Ok(_) => Ok(()),
                Err(ImapError::No(no)) if no.is_auth_failed() => {
                    Err(ImapError::AuthFailed(no.text))
                }
                Err(e) => Err(e),
            };
        }
        let (tag, bytes) = self.tags.build(&command::authenticate(mechanism));
        self.write_all(&bytes)?;
        let mut sent = false;
        loop {
            let resp = parse_response(&mut self.reader)?;
            match resp {
                Response::Continuation(_) => {
                    if sent {
                        self.write_all(b"\r\n")?;
                    } else {
                        self.write_all(&command::sasl_response(payload))?;
                        sent = true;
                    }
                }
                Response::Tagged { tag: t, line } if t == tag => {
                    self.update_capabilities_from_code(&line);
                    return self.finish_tagged(line);
                }
                Response::Untagged(Untagged::Capability(caps)) => {
                    self.capabilities = caps.into_iter().collect();
                }
                _ => {}
            }
        }
    }

    fn update_capabilities_from_code(&mut self, line: &StatusLine) {
        if line.code.as_deref() == Some("CAPABILITY")
            && let Some(args) = &line.code_args
        {
            self.capabilities = parse_caps(args);
        }
    }

    pub fn noop(&mut self) -> Result<CollectedResponse, ImapError> {
        self.run_collect(command::noop())
    }

    pub fn logout(&mut self) -> Result<(), ImapError> {
        if self.closed {
            return Ok(());
        }
        let _ = self.run_collect(command::logout());
        self.closed = true;
        Ok(())
    }

    pub fn run(&mut self, command: &str) -> Result<CollectedResponse, ImapError> {
        self.run_collect(command)
    }

    pub fn run_streamed<F>(
        &mut self,
        command: &str,
        mut on_untagged: F,
    ) -> Result<StatusLine, ImapError>
    where
        F: FnMut(Untagged),
    {
        let (tag, bytes) = self.tags.build(command);
        if command::contains_literal(command.as_bytes())
            && !self.has_capability("LITERAL+")
            && !self.has_capability("LITERAL-")
        {
            return Err(ImapError::Unsupported(
                "command contains a non-ASCII literal but server does not advertise LITERAL+ or LITERAL-"
                    .into(),
            ));
        }
        let started = Instant::now();
        self.write_all(&bytes)?;
        loop {
            let resp = parse_response(&mut self.reader)?;
            match resp {
                Response::Tagged { tag: t, line } if t == tag => {
                    self.update_capabilities_from_code(&line);
                    self.logger.trace_cmd(
                        "IMAP",
                        command,
                        &format!("{} {}", status_word(&line.status), line.text),
                        started.elapsed(),
                    );
                    return match line.status {
                        Status::Ok => Ok(line),
                        Status::No => Err(ImapError::No(line.into_no_error())),
                        Status::Bad => Err(ImapError::Bad(line.text)),
                        Status::Bye => Err(ImapError::Bye(line.text)),
                        Status::PreAuth => Err(ImapError::Protocol(
                            "unexpected PREAUTH on tagged response".into(),
                        )),
                    };
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
                Response::Untagged(u) => {
                    if let Untagged::Capability(caps) = &u {
                        self.capabilities = caps.iter().cloned().collect();
                    }
                    on_untagged(u);
                }
                Response::Continuation(_) => {
                    return Err(ImapError::Protocol(
                        "unexpected continuation outside AUTHENTICATE".into(),
                    ));
                }
            }
        }
    }

    pub fn run_collect(&mut self, command: &str) -> Result<CollectedResponse, ImapError> {
        let (tag, bytes) = self.tags.build(command);
        if command::contains_literal(command.as_bytes())
            && !self.has_capability("LITERAL+")
            && !self.has_capability("LITERAL-")
        {
            return Err(ImapError::Unsupported(
                "command contains a non-ASCII literal but server does not advertise LITERAL+ or LITERAL-"
                    .into(),
            ));
        }
        let started = Instant::now();
        self.write_all(&bytes)?;
        let mut untagged = Vec::new();
        loop {
            let resp = parse_response(&mut self.reader)?;
            match resp {
                Response::Tagged { tag: t, line } if t == tag => {
                    self.update_capabilities_from_code(&line);
                    self.logger.trace_cmd(
                        "IMAP",
                        command,
                        &format!("{} {}", status_word(&line.status), line.text),
                        started.elapsed(),
                    );
                    return match line.status {
                        Status::Ok => Ok(CollectedResponse {
                            tag,
                            line,
                            untagged,
                        }),
                        Status::No => Err(ImapError::No(line.into_no_error())),
                        Status::Bad => Err(ImapError::Bad(line.text)),
                        Status::Bye => Err(ImapError::Bye(line.text)),
                        Status::PreAuth => Err(ImapError::Protocol(
                            "unexpected PREAUTH on tagged response".into(),
                        )),
                    };
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
                Response::Untagged(u) => {
                    if let Untagged::Capability(caps) = &u {
                        self.capabilities = caps.iter().cloned().collect();
                    }
                    untagged.push(u);
                }
                Response::Continuation(_) => {
                    return Err(ImapError::Protocol(
                        "unexpected continuation outside AUTHENTICATE".into(),
                    ));
                }
            }
        }
    }

    fn finish_tagged(&mut self, line: StatusLine) -> Result<(), ImapError> {
        match line.status {
            Status::Ok => Ok(()),
            Status::No => {
                let no = NoError::new(line.text, line.code);
                if no.is_auth_failed() {
                    Err(ImapError::AuthFailed(no.text))
                } else {
                    Err(ImapError::No(no))
                }
            }
            Status::Bad => Err(ImapError::Bad(line.text)),
            Status::Bye => Err(ImapError::Bye(line.text)),
            Status::PreAuth => Err(ImapError::Protocol(
                "unexpected PREAUTH on tagged response".into(),
            )),
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
        if !self.closed {
            let _ = self.logout();
        }
    }
}

#[derive(Debug)]
pub struct CollectedResponse {
    pub tag: String,
    pub line: StatusLine,
    pub untagged: Vec<Untagged>,
}

fn status_word(status: &Status) -> &'static str {
    match status {
        Status::Ok => "OK",
        Status::No => "NO",
        Status::Bad => "BAD",
        Status::Bye => "BYE",
        Status::PreAuth => "PREAUTH",
    }
}

fn parse_caps(s: &str) -> BTreeSet<String> {
    s.split_ascii_whitespace().map(|w| w.to_owned()).collect()
}

fn noop_stream() -> Box<dyn ImapStream> {
    Box::new(NoopStream)
}

struct NoopStream;

impl Read for NoopStream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for NoopStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ImapStream for NoopStream {
    fn upgrade_tls(
        self: Box<Self>,
        _connector: &Connector,
        _host: &str,
    ) -> Result<Box<dyn ImapStream>, ImapError> {
        Ok(self)
    }
    fn is_tls(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct MockStream {
        reader: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl MockStream {
        fn boxed(server_says: &[u8]) -> Box<dyn ImapStream> {
            Box::new(MockStream {
                reader: Cursor::new(server_says.to_vec()),
                written: Vec::new(),
            })
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reader.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl ImapStream for MockStream {
        fn upgrade_tls(
            self: Box<Self>,
            _c: &Connector,
            _h: &str,
        ) -> Result<Box<dyn ImapStream>, ImapError> {
            Ok(self)
        }
        fn is_tls(&self) -> bool {
            false
        }
    }

    fn client_with(server: &[u8]) -> ImapClient {
        ImapClient {
            reader: BufReader::new(MockStream::boxed(server)),
            tags: CommandBuilder::new(),
            capabilities: BTreeSet::new(),
            host: "test.example".to_owned(),
            closed: false,
            utf8_accept: false,
            logger: Logger::from_flags(true, 0),
        }
    }

    #[test]
    fn run_collect_drains_untagged_and_returns_ok() {
        let server = b"* CAPABILITY IMAP4rev1 STARTTLS\r\nA0001 OK done\r\n";
        let mut c = client_with(server);
        let r = c.run_collect("CAPABILITY").unwrap();
        assert_eq!(r.untagged.len(), 1);
        assert!(matches!(r.line.status, Status::Ok));
    }

    #[test]
    fn run_collect_propagates_no_as_imaperror_no() {
        let server = b"A0001 NO mailbox not found\r\n";
        let mut c = client_with(server);
        let err = c.run_collect("SELECT bogus").unwrap_err();
        match err {
            ImapError::No(no) => assert_eq!(no.text, "mailbox not found"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn run_collect_propagates_bad_as_imaperror_bad() {
        let server = b"A0001 BAD syntax error\r\n";
        let mut c = client_with(server);
        let err = c.run_collect("BOGUS").unwrap_err();
        assert!(matches!(err, ImapError::Bad(_)));
    }

    #[test]
    fn run_collect_translates_authenticationfailed_via_login_path() {
        let server = b"A0001 NO [AUTHENTICATIONFAILED] bad creds\r\n";
        let mut c = client_with(server);
        let err = c.login("alice", "wrong").unwrap_err();
        assert!(matches!(err, ImapError::AuthFailed(_)));
    }

    #[test]
    fn login_refused_when_logindisabled() {
        let server = b"A0001 OK any\r\n";
        let mut c = client_with(server);
        c.capabilities.insert("LOGINDISABLED".to_owned());
        let err = c.login("a", "b").unwrap_err();
        assert!(matches!(err, ImapError::Unsupported(_)));
    }

    #[test]
    fn bye_mid_response_closes_client() {
        let server = b"* BYE going away\r\n";
        let mut c = client_with(server);
        let err = c.run_collect("NOOP").unwrap_err();
        assert!(matches!(err, ImapError::Bye(_)));
        assert!(c.closed);
    }

    #[test]
    fn authenticate_plain_sasl_ir_path() {
        let server = b"A0001 OK auth done\r\n";
        let mut c = client_with(server);
        c.capabilities.insert("SASL-IR".to_owned());
        c.authenticate_plain("alice", "p@ss").unwrap();
    }

    #[test]
    fn authenticate_plain_accepts_dovecot_post_login_untagged_capability() {
        let server = b"* CAPABILITY IMAP4rev1 SASL-IR MOVE IMAPSIEVE=sieve://127.0.0.1:4190 QUOTA\r\nA0001 OK [CAPABILITY IMAP4rev1 SASL-IR MOVE IMAPSIEVE=sieve://127.0.0.1:4190 QUOTA] Logged in\r\n";
        let mut c = client_with(server);
        c.capabilities.insert("SASL-IR".to_owned());
        c.authenticate_plain("alice", "p@ss").unwrap();
        assert!(c.has_capability("MOVE"));
        assert!(c.has_capability("IMAPSIEVE=sieve://127.0.0.1:4190"));
    }

    #[test]
    fn authenticate_plain_continuation_path() {
        let server = b"+ \r\nA0001 OK auth done\r\n";
        let mut c = client_with(server);
        c.authenticate_plain("alice", "p@ss").unwrap();
    }

    #[test]
    fn has_capability_is_case_insensitive() {
        let mut c = client_with(b"");
        c.capabilities.insert("STARTTLS".to_owned());
        assert!(c.has_capability("starttls"));
        assert!(c.has_capability("STARTTLS"));
        assert!(!c.has_capability("MOVE"));
    }
}
