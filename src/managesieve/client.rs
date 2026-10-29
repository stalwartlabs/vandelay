/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufRead, BufReader, Read, Write};
use std::time::Instant;

use encodify::base64::STANDARD;

use crate::imap::transport::{Connector, ImapStream};
use crate::logging::Logger;

use super::command;
use super::error::SieveError;
use super::response::{
    Capabilities, ResponseBlock, Status, StatusLine, Token, parse_capabilities, read_response,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMode {
    ImplicitTls,
    Plain,
    StartTls,
}

pub struct SieveClient {
    reader: BufReader<Box<dyn ImapStream>>,
    pub host: String,
    pub capabilities: Capabilities,
    closed: bool,
    fresh_post_auth_caps: bool,
    logger: Logger,
}

impl SieveClient {
    pub fn connect(
        connector: &Connector,
        host: &str,
        port: u16,
        mode: ConnectMode,
        allow_cleartext: bool,
        logger: Logger,
    ) -> Result<SieveClient, SieveError> {
        let stream: Box<dyn ImapStream> = match mode {
            ConnectMode::ImplicitTls => connector
                .connect_tls(host, port)
                .map_err(|e| SieveError::Tls(e.to_string()))?,
            ConnectMode::Plain | ConnectMode::StartTls => connector
                .connect_plain(host, port)
                .map_err(|e| SieveError::Io(std::io::Error::other(e.to_string())))?,
        };
        let mut client = SieveClient {
            reader: BufReader::new(stream),
            host: host.to_owned(),
            capabilities: Capabilities::default(),
            closed: false,
            fresh_post_auth_caps: false,
            logger,
        };
        client.read_initial_capabilities()?;
        if matches!(mode, ConnectMode::StartTls) {
            if client.capabilities.starttls {
                client.start_tls(connector)?;
                client.read_initial_capabilities()?;
            } else if !allow_cleartext {
                return Err(SieveError::Unsupported(
                    "server does not advertise STARTTLS; pass --allow-cleartext to permit \
                     credentials over an unencrypted socket"
                        .into(),
                ));
            }
        }
        Ok(client)
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn read_initial_capabilities(&mut self) -> Result<(), SieveError> {
        let block = read_response(&mut self.reader)?;
        match block.status.status {
            Status::Ok => {
                self.capabilities = parse_capabilities(&block.data);
                Ok(())
            }
            Status::Bye => {
                self.closed = true;
                Err(SieveError::Bye(block.status.text))
            }
            Status::No => Err(SieveError::No(block.status.into_no_error())),
        }
    }

    pub fn refresh_capabilities(&mut self) -> Result<(), SieveError> {
        let block = self.run(&command::capability())?;
        let caps = parse_capabilities(&block.data);
        self.capabilities = caps;
        self.fresh_post_auth_caps = true;
        Ok(())
    }

    pub fn had_fresh_post_auth_caps(&self) -> bool {
        self.fresh_post_auth_caps
    }

    fn consume_post_auth_caps(&mut self, data: &[Vec<super::response::Token>]) {
        if data.is_empty() {
            return;
        }
        let parsed = parse_capabilities(data);
        let mentions_any =
            !parsed.sasl.is_empty() || parsed.implementation.is_some() || parsed.version.is_some();
        if mentions_any {
            self.capabilities = parsed;
            self.fresh_post_auth_caps = true;
        }
    }

    fn start_tls(&mut self, connector: &Connector) -> Result<(), SieveError> {
        let block = self.run(&command::starttls())?;
        let _ = block;
        let stream = self.reader.get_ref();
        if stream.is_tls() {
            return Err(SieveError::Protocol(
                "STARTTLS issued but stream already TLS".into(),
            ));
        }
        let buffered = std::mem::replace(&mut self.reader, BufReader::new(noop_stream()));
        if !buffered.buffer().is_empty() {
            return Err(SieveError::Protocol(
                "buffered data found after STARTTLS OK; possible response injection".into(),
            ));
        }
        let inner = buffered.into_inner();
        let upgraded = inner
            .upgrade_tls(connector, &self.host)
            .map_err(|e| SieveError::Tls(e.to_string()))?;
        self.reader = BufReader::new(upgraded);
        Ok(())
    }

    pub fn run(&mut self, command: &str) -> Result<ResponseBlock, SieveError> {
        let started = Instant::now();
        self.write_all(command.as_bytes())?;
        let block = read_response(&mut self.reader)?;
        self.logger.trace_cmd(
            "SIEVE",
            command,
            &sieve_status(&block.status),
            started.elapsed(),
        );
        finish_block(block, &mut self.closed)
    }

    pub fn run_raw(&mut self, bytes: &[u8]) -> Result<ResponseBlock, SieveError> {
        let started = Instant::now();
        self.write_all(bytes)?;
        let block = read_response(&mut self.reader)?;
        self.logger.trace_cmd(
            "SIEVE",
            &String::from_utf8_lossy(bytes),
            &sieve_status(&block.status),
            started.elapsed(),
        );
        finish_block(block, &mut self.closed)
    }

    pub fn listscripts(&mut self) -> Result<ResponseBlock, SieveError> {
        self.run(&command::listscripts())
    }

    pub fn getscript(&mut self, name: &str) -> Result<ResponseBlock, SieveError> {
        self.run(&command::getscript(name))
    }

    pub fn noop(&mut self) -> Result<(), SieveError> {
        let _ = self.run(&command::noop())?;
        Ok(())
    }

    pub fn logout(&mut self) -> Result<(), SieveError> {
        if self.closed {
            return Ok(());
        }
        let _ = self.write_all(command::logout().as_bytes());
        self.closed = true;
        Ok(())
    }

    pub fn authenticate_plain(&mut self, authcid: &str, password: &str) -> Result<(), SieveError> {
        let mut payload: Vec<u8> = Vec::with_capacity(authcid.len() + password.len() + 3);
        payload.push(0);
        payload.extend_from_slice(authcid.as_bytes());
        payload.push(0);
        payload.extend_from_slice(password.as_bytes());
        let encoded = STANDARD.encode(&payload);
        self.send_authenticate("PLAIN", &encoded)
    }

    pub fn authenticate_login(&mut self, authcid: &str, password: &str) -> Result<(), SieveError> {
        self.fresh_post_auth_caps = false;
        self.write_all(command::authenticate("LOGIN").as_bytes())?;
        let user_payload = STANDARD.encode(authcid);
        self.expect_continuation()?;
        self.write_all(command::continuation_payload(&user_payload).as_bytes())?;
        let pass_payload = STANDARD.encode(password);
        self.expect_continuation()?;
        self.write_all(command::continuation_payload(&pass_payload).as_bytes())?;
        let block = read_response(&mut self.reader)?;
        match finish_block(block, &mut self.closed) {
            Ok(block) => {
                self.consume_post_auth_caps(&block.data);
                Ok(())
            }
            Err(SieveError::No(no)) if no.is_referral() => {
                Err(SieveError::Referral(no.to_string()))
            }
            Err(other) => Err(other),
        }
    }

    pub fn authenticate_oauthbearer(&mut self, user: &str, token: &str) -> Result<(), SieveError> {
        let mut payload = String::with_capacity(user.len() + token.len() + 32);
        payload.push_str("n,a=");
        payload.push_str(user);
        payload.push(',');
        payload.push('\x01');
        payload.push_str("auth=Bearer ");
        payload.push_str(token);
        payload.push('\x01');
        payload.push('\x01');
        let encoded = STANDARD.encode(&payload);
        self.send_authenticate("OAUTHBEARER", &encoded)
    }

    fn send_authenticate(&mut self, mechanism: &str, b64: &str) -> Result<(), SieveError> {
        self.fresh_post_auth_caps = false;
        self.write_all(command::authenticate_with_initial(mechanism, b64).as_bytes())?;
        let block = read_response(&mut self.reader)?;
        match finish_block(block, &mut self.closed) {
            Ok(block) => {
                self.consume_post_auth_caps(&block.data);
                Ok(())
            }
            Err(SieveError::No(no)) if no.is_referral() => {
                Err(SieveError::Referral(no.to_string()))
            }
            Err(other) => Err(other),
        }
    }

    fn expect_continuation(&mut self) -> Result<(), SieveError> {
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = self.reader.read_until(b'\n', &mut line)?;
            if n == 0 {
                return Err(SieveError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "expected continuation",
                )));
            }
            while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let s = String::from_utf8_lossy(&line);
            let trimmed = s.trim_start();
            if trimmed.starts_with('"') || trimmed.starts_with('{') {
                return Ok(());
            }
            if trimmed.starts_with("NO") || trimmed.starts_with("BYE") {
                let line_tokens = tokens_from_text(&line)?;
                let status = super::response::try_parse_status(&line_tokens)
                    .ok_or_else(|| SieveError::Parse(format!("bad status: {s:?}")))?;
                let block = ResponseBlock {
                    data: Vec::new(),
                    status,
                };
                return finish_block(block, &mut self.closed).map(|_| ());
            }
            return Err(SieveError::Parse(format!(
                "unexpected line during AUTHENTICATE LOGIN: {s:?}"
            )));
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), SieveError> {
        let stream = self.reader.get_mut();
        stream.write_all(bytes)?;
        stream.flush()?;
        Ok(())
    }
}

impl Drop for SieveClient {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.logout();
        }
    }
}

impl SieveClient {
    pub fn new_from_stream(stream: Box<dyn ImapStream>, host: String) -> Self {
        SieveClient {
            reader: BufReader::new(stream),
            host,
            capabilities: Capabilities::default(),
            closed: false,
            fresh_post_auth_caps: false,
            logger: Logger::from_flags(true, 0),
        }
    }

    pub fn read_response_block(&mut self) -> Result<ResponseBlock, SieveError> {
        let block = read_response(&mut self.reader)?;
        finish_block(block, &mut self.closed)
    }
}

fn tokens_from_text(line: &[u8]) -> Result<Vec<Token>, SieveError> {
    let mut buf: Vec<u8> = line.to_vec();
    buf.extend_from_slice(b"\r\n");
    let mut cursor = std::io::Cursor::new(buf);
    let mut reader = BufReader::new(&mut cursor);
    let block = read_response_with_eof_ok(&mut reader)?;
    if let Some(line) = block.data.into_iter().next() {
        return Ok(line);
    }
    Ok(Vec::new())
}

fn read_response_with_eof_ok<R: std::io::BufRead>(
    reader: &mut R,
) -> Result<ResponseBlock, SieveError> {
    match read_response(reader) {
        Ok(b) => Ok(b),
        Err(SieveError::Io(_)) => Ok(ResponseBlock::default()),
        Err(e) => Err(e),
    }
}

fn finish_block(block: ResponseBlock, closed: &mut bool) -> Result<ResponseBlock, SieveError> {
    match block.status.status {
        Status::Ok => Ok(block),
        Status::No => {
            let no = StatusLine::into_no_error(block.status);
            Err(SieveError::No(no))
        }
        Status::Bye => {
            *closed = true;
            Err(SieveError::Bye(block.status.text))
        }
    }
}

fn sieve_status(status: &StatusLine) -> String {
    let word = match status.status {
        Status::Ok => "OK",
        Status::No => "NO",
        Status::Bye => "BYE",
    };
    format!("{word} {}", status.text)
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
    ) -> Result<Box<dyn ImapStream>, crate::imap::ImapError> {
        Ok(self)
    }
    fn is_tls(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap::ImapError;
    use std::io::Cursor;

    struct MockStream {
        reader: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl MockStream {
        fn new(server_says: &[u8]) -> Self {
            MockStream {
                reader: Cursor::new(server_says.to_vec()),
                written: Vec::new(),
            }
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

    fn client_with(server: &[u8]) -> SieveClient {
        SieveClient::new_from_stream(Box::new(MockStream::new(server)), "test.example".to_owned())
    }

    #[test]
    fn run_listscripts_returns_data_and_ok() {
        let server = b"\"a\"\r\n\"b\" ACTIVE\r\nOK\r\n";
        let mut c = client_with(server);
        let r = c.listscripts().unwrap();
        let scripts = super::super::response::parse_listscripts(&r.data).unwrap();
        assert_eq!(scripts.len(), 2);
        assert!(scripts[1].active);
    }

    #[test]
    fn run_returns_no_as_sieve_error_no() {
        let server = b"NO (NONEXISTENT) \"not here\"\r\n";
        let mut c = client_with(server);
        let err = c.getscript("missing").unwrap_err();
        match err {
            SieveError::No(no) => {
                assert_eq!(no.code.as_deref(), Some("NONEXISTENT"));
                assert!(no.is_nonexistent());
            }
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn bye_closes_client() {
        let server = b"BYE \"going away\"\r\n";
        let mut c = client_with(server);
        let err = c.noop().unwrap_err();
        assert!(matches!(err, SieveError::Bye(_)));
        assert!(c.is_closed());
    }

    #[test]
    fn authenticate_plain_sends_initial_response() {
        let server = b"OK\r\n";
        let mut c = client_with(server);
        c.authenticate_plain("alice", "p@ss").unwrap();
    }

    #[test]
    fn authenticate_oauthbearer_sends_initial_response() {
        let server = b"OK\r\n";
        let mut c = client_with(server);
        c.authenticate_oauthbearer("alice@x", "tok").unwrap();
    }

    #[test]
    fn authenticate_login_walks_two_continuations() {
        let server = b"\"VXNlcm5hbWU6\"\r\n\"UGFzc3dvcmQ6\"\r\nOK\r\n";
        let mut c = client_with(server);
        c.authenticate_login("alice", "p@ss").unwrap();
    }

    #[test]
    fn authenticate_no_response_propagates_as_no() {
        let server = b"NO \"bad creds\"\r\n";
        let mut c = client_with(server);
        let err = c.authenticate_plain("alice", "wrong").unwrap_err();
        assert!(matches!(err, SieveError::No(_)), "got {err:?}");
    }

    #[test]
    fn authenticate_referral_no_translates_to_referral() {
        let server = b"NO (REFERRAL \"sieve://other\") \"go\"\r\n";
        let mut c = client_with(server);
        let err = c.authenticate_plain("alice", "x").unwrap_err();
        assert!(matches!(err, SieveError::Referral(_)), "got {err:?}");
    }
}
