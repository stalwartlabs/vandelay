/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io;
use std::time::Duration;

use encodify::base64::STANDARD as B64;
use ureq::Agent;
use ureq::http::{Method, Request};

use super::error::{ContainerError, ContainerResult};

pub struct DavSeed {
    agent: Agent,
    base: String,
    auth: String,
}

impl DavSeed {
    pub fn new(base: impl Into<String>, user: &str, password: &str) -> Self {
        super::install_crypto_provider();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .new_agent();
        let credentials = B64.encode(format!("{user}:{password}"));
        Self {
            agent,
            base: base.into(),
            auth: format!("Basic {credentials}"),
        }
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_owned()
        } else if path.starts_with('/') {
            format!("{}{path}", self.base.trim_end_matches('/'))
        } else {
            format!("{}/{}", self.base.trim_end_matches('/'), path)
        }
    }

    fn run(
        &self,
        method: &str,
        url: &str,
        depth: Option<u8>,
        content_type: Option<&str>,
        body: Option<&[u8]>,
    ) -> ContainerResult<(u16, Vec<u8>)> {
        let parsed = Method::from_bytes(method.as_bytes())
            .map_err(|e| ContainerError::Protocol(format!("bad method {method}: {e}")))?;
        let payload: Vec<u8> = body.map(|b| b.to_vec()).unwrap_or_default();
        let mut attempt = 0u32;
        loop {
            let mut builder = Request::builder()
                .method(parsed.clone())
                .uri(url)
                .header("Authorization", &self.auth);
            if let Some(d) = depth {
                builder = builder.header("Depth", d.to_string());
            }
            if let Some(ct) = content_type {
                builder = builder.header("Content-Type", ct);
            }
            let request = builder
                .body(payload.clone())
                .map_err(|e| ContainerError::Protocol(format!("build request: {e}")))?;
            match self.agent.run(request) {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    let bytes = response.body_mut().read_to_vec()?;
                    return Ok((status, bytes));
                }
                Err(e) if attempt < 20 && is_transient(&e) => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn mkcol(&self, path: &str, body: Option<&str>) -> ContainerResult<u16> {
        self.mkcol_with_method("MKCOL", path, body)
    }

    pub fn mkcalendar(&self, path: &str, body: Option<&str>) -> ContainerResult<u16> {
        self.mkcol_with_method("MKCALENDAR", path, body)
    }

    fn mkcol_with_method(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> ContainerResult<u16> {
        let url = self.url(path);
        let bytes = body.map(|b| b.as_bytes());
        let (status, response_body) = self.run(
            method,
            &url,
            None,
            Some("application/xml; charset=utf-8"),
            bytes,
        )?;
        if !is_collection_created(status) {
            return Err(ContainerError::Protocol(format!(
                "{method} {url} -> {status}: {}",
                truncate(&response_body)
            )));
        }
        Ok(status)
    }

    pub fn put(&self, path: &str, content_type: &str, body: &[u8]) -> ContainerResult<u16> {
        let url = self.url(path);
        let (status, response_body) =
            self.run("PUT", &url, None, Some(content_type), Some(body))?;
        if !(200..300).contains(&status) {
            return Err(ContainerError::Protocol(format!(
                "PUT {url} -> {status}: {}",
                truncate(&response_body)
            )));
        }
        Ok(status)
    }

    pub fn delete(&self, path: &str) -> ContainerResult<u16> {
        let url = self.url(path);
        let (status, response_body) = self.run("DELETE", &url, None, None, None)?;
        if !(200..300).contains(&status) && status != 404 {
            return Err(ContainerError::Protocol(format!(
                "DELETE {url} -> {status}: {}",
                truncate(&response_body)
            )));
        }
        Ok(status)
    }

    pub fn propfind(&self, path: &str, depth: u8) -> ContainerResult<String> {
        let url = self.url(path);
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop><resourcetype/><displayname/></prop></propfind>"#;
        let (status, bytes) = self.run(
            "PROPFIND",
            &url,
            Some(depth),
            Some("application/xml; charset=utf-8"),
            Some(body.as_bytes()),
        )?;
        if !(200..400).contains(&status) {
            return Err(ContainerError::Protocol(format!(
                "PROPFIND {url} -> {status}: {}",
                truncate(&bytes)
            )));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn is_transient(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(io) => matches!(
            io.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

fn is_collection_created(status: u16) -> bool {
    matches!(status, 200 | 201 | 204)
}

fn truncate(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    if s.len() > 2000 {
        format!("{}…", &s[..2000])
    } else {
        s.into_owned()
    }
}
