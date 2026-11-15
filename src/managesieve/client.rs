/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Write};
use std::time::Instant;

use encodify::base64::STANDARD;

use crate::logging::Logger;
use crate::net::sasl::SaslPayload;
use crate::net::stream::{Cleartext, ConnectMode, Connector, Stream};

use super::command::Command;
use super::error::SieveError;
use super::response::{Capabilities, ResponseBlock, Status, StatusLine, Token};

const LOGIN: &str = "LOGIN";

pub struct SieveClient {
    reader: BufReader<Stream>,
    host: String,
    capabilities: Capabilities,
    closed: bool,
    fresh_post_auth_caps: bool,
    logger: Logger,
    wire: String,
}

impl SieveClient {
    pub fn connect(
        connector: &Connector,
        host: &str,
        port: u16,
        mode: ConnectMode,
        cleartext: Cleartext,
        logger: Logger,
    ) -> Result<SieveClient, SieveError> {
        let stream = match mode {
            ConnectMode::ImplicitTls => connector
                .connect_tls(host, port)
                .map_err(|e| SieveError::Tls(e.to_string()))?,
            ConnectMode::Plain | ConnectMode::StartTls => connector
                .connect_plain(host, port)
                .map_err(|e| SieveError::Io(io::Error::other(e.to_string())))?,
        };
        let mut client = SieveClient::from_stream(stream, host.to_owned(), logger);
        client.read_initial_capabilities()?;
        if mode == ConnectMode::StartTls {
            if client.capabilities.starttls {
                client.start_tls(connector)?;
                client.read_initial_capabilities()?;
            } else if cleartext == Cleartext::Forbid {
                return Err(SieveError::Unsupported(
                    "server does not advertise STARTTLS; pass --allow-cleartext to permit \
                     credentials over an unencrypted socket"
                        .into(),
                ));
            }
        }
        Ok(client)
    }

    fn from_stream(stream: Stream, host: String, logger: Logger) -> SieveClient {
        SieveClient {
            reader: BufReader::new(stream),
            host,
            capabilities: Capabilities::default(),
            closed: false,
            fresh_post_auth_caps: false,
            logger,
            wire: String::with_capacity(64),
        }
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn read_initial_capabilities(&mut self) -> Result<(), SieveError> {
        let block = ResponseBlock::read(&mut self.reader)?;
        match block.status.status {
            Status::Ok => {
                self.capabilities = Capabilities::parse(&block.data);
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
        let block = self.run(Command::Capability)?;
        self.capabilities = Capabilities::parse(&block.data);
        self.fresh_post_auth_caps = true;
        Ok(())
    }

    pub fn had_fresh_post_auth_caps(&self) -> bool {
        self.fresh_post_auth_caps
    }

    fn consume_post_auth_caps(&mut self, data: &[Vec<Token>]) {
        if data.is_empty() {
            return;
        }
        let parsed = Capabilities::parse(data);
        if !parsed.sasl.is_empty() || parsed.implementation.is_some() || parsed.version.is_some() {
            self.capabilities = parsed;
            self.fresh_post_auth_caps = true;
        }
    }

    fn start_tls(&mut self, connector: &Connector) -> Result<(), SieveError> {
        self.run(Command::StartTls)?;
        if self.reader.get_ref().is_tls() {
            return Err(SieveError::Protocol(
                "STARTTLS issued but stream already TLS".into(),
            ));
        }
        if !self.reader.buffer().is_empty() {
            return Err(SieveError::Protocol(
                "buffered data found after STARTTLS OK; possible response injection".into(),
            ));
        }
        let upgraded = self
            .reader
            .get_mut()
            .take()
            .upgrade_tls(connector, &self.host)
            .map_err(|e| SieveError::Tls(e.to_string()))?;
        self.reader = BufReader::new(upgraded);
        Ok(())
    }

    fn run(&mut self, command: Command<'_>) -> Result<ResponseBlock, SieveError> {
        let started = Instant::now();
        self.send(command)?;
        let block = ResponseBlock::read(&mut self.reader)?;
        self.logger.trace_cmd(
            "SIEVE",
            &self.wire,
            &block.status.to_string(),
            started.elapsed(),
        );
        self.finish(block)
    }

    pub fn listscripts(&mut self) -> Result<ResponseBlock, SieveError> {
        self.run(Command::ListScripts)
    }

    pub fn getscript(&mut self, name: &str) -> Result<Vec<u8>, SieveError> {
        self.run(Command::GetScript(name))?.into_script()
    }

    pub fn logout(&mut self) {
        if !self.closed {
            let _ = self.send(Command::Logout);
            self.closed = true;
        }
    }

    pub fn authenticate(&mut self, payload: SaslPayload<'_>) -> Result<(), SieveError> {
        self.fresh_post_auth_caps = false;
        let initial = payload.base64();
        self.send(Command::Authenticate {
            mechanism: payload.mechanism(),
            initial: Some(&initial),
        })?;
        let block = ResponseBlock::read(&mut self.reader)?;
        self.finish_authentication(block)
    }

    pub fn authenticate_login(&mut self, authcid: &str, password: &str) -> Result<(), SieveError> {
        self.fresh_post_auth_caps = false;
        self.send(Command::Authenticate {
            mechanism: LOGIN,
            initial: None,
        })?;
        for secret in [authcid, password] {
            self.expect_continuation()?;
            self.send(Command::Continuation(&STANDARD.encode(secret)))?;
        }
        let block = ResponseBlock::read(&mut self.reader)?;
        self.finish_authentication(block)
    }

    fn finish_authentication(&mut self, block: ResponseBlock) -> Result<(), SieveError> {
        match self.finish(block) {
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
            if self.reader.read_until(b'\n', &mut line)? == 0 {
                return Err(SieveError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "expected continuation",
                )));
            }
            while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                line.pop();
            }
            let start = line
                .iter()
                .position(|b| !b.is_ascii_whitespace())
                .unwrap_or(line.len());
            let trimmed = line.get(start..).unwrap_or_default();
            if line.is_empty() {
                continue;
            }
            if trimmed.starts_with(b"\"") || trimmed.starts_with(b"{") {
                return Ok(());
            }
            if trimmed.starts_with(b"NO") || trimmed.starts_with(b"BYE") {
                let status = StatusLine::from_line(&line)?.ok_or_else(|| {
                    SieveError::Parse(format!("bad status: {:?}", String::from_utf8_lossy(&line)))
                })?;
                return self
                    .finish(ResponseBlock {
                        data: Vec::new(),
                        status,
                    })
                    .map(drop);
            }
            return Err(SieveError::Parse(format!(
                "unexpected line during AUTHENTICATE LOGIN: {:?}",
                String::from_utf8_lossy(&line)
            )));
        }
    }

    fn finish(&mut self, block: ResponseBlock) -> Result<ResponseBlock, SieveError> {
        match block.status.status {
            Status::Ok => Ok(block),
            Status::No => Err(SieveError::No(block.status.into_no_error())),
            Status::Bye => {
                self.closed = true;
                Err(SieveError::Bye(block.status.text))
            }
        }
    }

    fn send(&mut self, command: Command<'_>) -> Result<(), SieveError> {
        self.wire.clear();
        let _ = write!(self.wire, "{command}");
        let stream = self.reader.get_mut();
        stream.write_all(self.wire.as_bytes())?;
        stream.flush()?;
        Ok(())
    }
}

impl Drop for SieveClient {
    fn drop(&mut self) {
        self.logout();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::Level;
    use crate::managesieve::response::ListedScript;

    fn client_with(server: &[u8]) -> SieveClient {
        SieveClient::from_stream(
            Stream::mock(server),
            "test.example".to_owned(),
            Logger::new(Level::Quiet),
        )
    }

    #[test]
    fn run_listscripts_returns_data_and_ok() {
        let server = b"\"a\"\r\n\"b\" ACTIVE\r\nOK\r\n";
        let mut c = client_with(server);
        let block = c.listscripts().unwrap();
        let scripts = ListedScript::parse_all(&block.data).unwrap();
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
            }
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn bye_closes_client() {
        let server = b"BYE \"going away\"\r\n";
        let mut c = client_with(server);
        let err = c.listscripts().unwrap_err();
        assert!(matches!(err, SieveError::Bye(_)));
        assert!(c.closed);
    }

    #[test]
    fn authenticate_plain_sends_initial_response() {
        let server = b"OK\r\n";
        let mut c = client_with(server);
        c.authenticate(SaslPayload::Plain {
            authcid: "alice",
            password: "p@ss",
        })
        .unwrap();
    }

    #[test]
    fn authenticate_oauthbearer_sends_initial_response() {
        let server = b"OK\r\n";
        let mut c = client_with(server);
        c.authenticate(SaslPayload::OAuthBearer {
            user: "alice@x",
            token: "tok",
        })
        .unwrap();
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
        let err = c
            .authenticate(SaslPayload::Plain {
                authcid: "alice",
                password: "wrong",
            })
            .unwrap_err();
        assert!(matches!(err, SieveError::No(_)), "got {err:?}");
    }

    #[test]
    fn authenticate_referral_no_translates_to_referral() {
        let server = b"NO (REFERRAL \"sieve://other\") \"go\"\r\n";
        let mut c = client_with(server);
        let err = c
            .authenticate(SaslPayload::Plain {
                authcid: "alice",
                password: "x",
            })
            .unwrap_err();
        assert!(matches!(err, SieveError::Referral(_)), "got {err:?}");
    }

    #[test]
    fn authenticate_login_no_at_a_continuation_is_a_no() {
        // a NO in place of a LOGIN challenge surfaces as SieveError::No, not a parse error
        let server = b"NO \"bad creds\"\r\n";
        let mut c = client_with(server);
        let err = c.authenticate_login("alice", "wrong").unwrap_err();
        assert!(matches!(err, SieveError::No(_)), "got {err:?}");
    }

    #[test]
    fn plain_initial_response_is_the_sasl_payload() {
        let mut c = client_with(b"OK\r\n");
        c.authenticate(SaslPayload::Plain {
            authcid: "alice",
            password: "p@ss",
        })
        .unwrap();
        assert_eq!(c.wire, "AUTHENTICATE \"PLAIN\" \"AGFsaWNlAHBAc3M=\"\r\n");
    }
}
