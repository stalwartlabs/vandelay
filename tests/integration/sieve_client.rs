/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use encodify::base64::STANDARD as B64;

use super::error::{ContainerError, ContainerResult};

pub struct SieveSeed {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl SieveSeed {
    pub fn connect_seed(host: &str, port: u16) -> ContainerResult<Self> {
        Self::connect(host, port)
    }

    pub fn connect(host: &str, port: u16) -> ContainerResult<Self> {
        let stream = TcpStream::connect((host, port))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let writer = stream.try_clone()?;
        let mut me = Self {
            reader: BufReader::new(stream),
            writer,
        };
        me.read_until_ok()?;
        Ok(me)
    }

    fn read_line(&mut self) -> ContainerResult<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(ContainerError::Protocol("sieve eof".to_owned()));
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(line)
    }

    fn read_until_ok(&mut self) -> ContainerResult<Vec<String>> {
        let mut lines = Vec::new();
        loop {
            let line = self.read_line()?;
            if line.starts_with("OK") {
                return Ok(lines);
            }
            if line.starts_with("NO") || line.starts_with("BYE") {
                return Err(ContainerError::Protocol(format!("sieve: {line}")));
            }
            lines.push(line);
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> ContainerResult<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn authenticate(&mut self, user: &str, password: &str) -> ContainerResult<()> {
        let mut blob = Vec::new();
        blob.push(0u8);
        blob.extend_from_slice(user.as_bytes());
        blob.push(0u8);
        blob.extend_from_slice(password.as_bytes());
        let encoded = B64.encode(&blob);
        let cmd = format!(
            "AUTHENTICATE \"PLAIN\" {{{}+}}\r\n{encoded}\r\n",
            encoded.len()
        );
        self.write_all(cmd.as_bytes())?;
        self.read_until_ok()?;
        Ok(())
    }

    pub fn putscript(&mut self, name: &str, body: &str) -> ContainerResult<()> {
        let cmd = format!(
            "PUTSCRIPT \"{}\" {{{}+}}\r\n{}\r\n",
            escape(name),
            body.len(),
            body
        );
        self.write_all(cmd.as_bytes())?;
        self.read_until_ok()?;
        Ok(())
    }

    pub fn putscript_raw(&mut self, name: &str, body: &str) -> ContainerResult<bool> {
        let cmd = format!(
            "PUTSCRIPT \"{}\" {{{}+}}\r\n{}\r\n",
            escape(name),
            body.len(),
            body
        );
        self.write_all(cmd.as_bytes())?;
        match self.read_until_ok() {
            Ok(_) => Ok(true),
            Err(ContainerError::Protocol(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn setactive(&mut self, name: &str) -> ContainerResult<()> {
        self.write_all(format!("SETACTIVE \"{}\"\r\n", escape(name)).as_bytes())?;
        self.read_until_ok()?;
        Ok(())
    }

    pub fn listscripts(&mut self) -> ContainerResult<Vec<(String, bool)>> {
        self.write_all(b"LISTSCRIPTS\r\n")?;
        let lines = self.read_until_ok()?;
        let mut scripts = Vec::new();
        for line in lines {
            let trimmed = line.trim();
            if let Some(stripped) = trimmed.strip_prefix('"') {
                let close = stripped.find('"').unwrap_or(stripped.len());
                let name = stripped[..close].to_owned();
                let rest = &stripped[close..];
                let active = rest.contains("ACTIVE");
                scripts.push((name, active));
            }
        }
        Ok(scripts)
    }

    pub fn logout(&mut self) -> ContainerResult<()> {
        let _ = self.write_all(b"LOGOUT\r\n");
        let mut buf = String::new();
        let _ = self.reader.read_to_string(&mut buf);
        Ok(())
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
