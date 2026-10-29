/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::time::Duration;

pub const LEVEL_QUIET: u8 = 0;
pub const LEVEL_DEFAULT: u8 = 1;
pub const LEVEL_PROGRESS: u8 = 2;
pub const LEVEL_METHOD: u8 = 3;
pub const LEVEL_BODIES: u8 = 4;

pub const TRACE_BODY_CAP: usize = 8192;

#[derive(Debug, Clone, Copy)]
pub struct Logger {
    level: u8,
}

impl Logger {
    pub fn new(level: u8) -> Self {
        Logger {
            level: level.min(LEVEL_BODIES),
        }
    }

    pub fn from_flags(quiet: bool, verbose: u8) -> Self {
        let level = if quiet {
            LEVEL_QUIET
        } else {
            LEVEL_DEFAULT.saturating_add(verbose)
        };
        Logger::new(level)
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn enabled(&self, level: u8) -> bool {
        self.level >= level
    }

    pub fn warn(&self, message: &str) {
        eprintln!("warning: {message}");
    }

    pub fn error(&self, message: &str) {
        eprintln!("error: {message}");
    }

    pub fn trace_http(&self, call: &HttpCall<'_>) {
        if !self.enabled(LEVEL_METHOD) {
            return;
        }
        match call.note {
            Some(note) => eprintln!(
                "{} {} {} -> {} ({} ms) {note}",
                call.proto,
                call.method,
                call.url,
                call.status,
                call.elapsed.as_millis()
            ),
            None => eprintln!(
                "{} {} {} -> {} ({} ms)",
                call.proto,
                call.method,
                call.url,
                call.status,
                call.elapsed.as_millis()
            ),
        }
        if self.enabled(LEVEL_BODIES) {
            if let Some(request) = call.request {
                trace_body('>', request, call.request_type);
            }
            trace_body('<', call.response, call.response_type);
        }
    }

    pub fn trace_cmd(&self, proto: &str, command: &str, status: &str, elapsed: Duration) {
        if !self.enabled(LEVEL_METHOD) {
            return;
        }
        let command = cap(&redact_wire(command), command.len());
        eprintln!("{proto} {command} -> {status} ({} ms)", elapsed.as_millis());
    }

    pub fn trace_http_error(
        &self,
        proto: &str,
        method: &str,
        url: &str,
        error: &str,
        elapsed: Duration,
    ) {
        if !self.enabled(LEVEL_METHOD) {
            return;
        }
        eprintln!(
            "{proto} {method} {url} -> transport error: {error} ({} ms)",
            elapsed.as_millis()
        );
    }
}

pub struct HttpCall<'a> {
    pub proto: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub status: u16,
    pub elapsed: Duration,
    pub note: Option<&'a str>,
    pub request: Option<&'a [u8]>,
    pub request_type: Option<&'a str>,
    pub response: &'a [u8],
    pub response_type: Option<&'a str>,
}

fn trace_body(marker: char, body: &[u8], content_type: Option<&str>) {
    for line in body_for_log(body, content_type).lines() {
        eprintln!("  {marker} {line}");
    }
}

pub fn body_for_log(body: &[u8], content_type: Option<&str>) -> String {
    if body.is_empty() {
        return "<empty body>".to_owned();
    }
    let is_json = content_type.is_some_and(|c| c.to_ascii_lowercase().contains("json"));
    if is_json
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
        && let Ok(pretty) = serde_json::to_string_pretty(&value)
    {
        return cap(&pretty, body.len());
    }
    let head = safe_head(body, TRACE_BODY_CAP);
    match std::str::from_utf8(head) {
        Ok(text) if is_texty(text) => {
            if body.len() > head.len() {
                format!("{text}\n... <{} bytes total>", body.len())
            } else {
                text.to_owned()
            }
        }
        _ => blob_summary(body),
    }
}

fn safe_head(body: &[u8], max: usize) -> &[u8] {
    if body.len() <= max {
        return body;
    }
    let mut end = max;
    while end > 0 && (body[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    &body[..end]
}

fn is_texty(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

fn blob_summary(body: &[u8]) -> String {
    format!(
        "<{} bytes, blake3={}>",
        body.len(),
        blake3::hash(body).to_hex()
    )
}

fn cap(text: &str, total_bytes: usize) -> String {
    if text.len() <= TRACE_BODY_CAP {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(TRACE_BODY_CAP);
    format!("{}\n... <{total_bytes} bytes total>", &text[..end])
}

pub fn redact_wire(command: &str) -> String {
    let trimmed = command.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.splitn(3, ' ');
    let first = parts.next().unwrap_or("");
    if first.eq_ignore_ascii_case("LOGIN") {
        return format!("{first} <redacted>");
    }
    if first.eq_ignore_ascii_case("AUTHENTICATE") {
        let mechanism = parts.next().unwrap_or("");
        return match parts.next() {
            Some(_) => format!("{first} {mechanism} <redacted>"),
            None => format!("{first} {mechanism}").trim_end().to_owned(),
        };
    }
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_is_level_zero() {
        assert_eq!(Logger::from_flags(true, 3).level(), LEVEL_QUIET);
    }

    #[test]
    fn default_is_level_one() {
        assert_eq!(Logger::from_flags(false, 0).level(), LEVEL_DEFAULT);
    }

    #[test]
    fn verbose_caps_at_four() {
        assert_eq!(Logger::from_flags(false, 9).level(), LEVEL_BODIES);
    }

    #[test]
    fn enabled_is_a_superset_scale() {
        let l = Logger::from_flags(false, 1);
        assert!(l.enabled(LEVEL_DEFAULT));
        assert!(l.enabled(LEVEL_PROGRESS));
        assert!(!l.enabled(LEVEL_METHOD));
    }

    #[test]
    fn redact_wire_hides_login_arguments() {
        assert_eq!(
            redact_wire("LOGIN \"alice\" \"s3cret\""),
            "LOGIN <redacted>"
        );
    }

    #[test]
    fn redact_wire_hides_authenticate_initial_response() {
        assert_eq!(
            redact_wire("AUTHENTICATE PLAIN AGFsaWNlAHMzY3JldA=="),
            "AUTHENTICATE PLAIN <redacted>"
        );
    }

    #[test]
    fn redact_wire_keeps_authenticate_without_initial_response() {
        assert_eq!(redact_wire("AUTHENTICATE PLAIN"), "AUTHENTICATE PLAIN");
    }

    #[test]
    fn redact_wire_passes_through_ordinary_commands() {
        assert_eq!(redact_wire("SELECT INBOX\r\n"), "SELECT INBOX");
    }

    #[test]
    fn body_for_log_pretty_prints_json() {
        let out = body_for_log(br#"{"a":1}"#, Some("application/json"));
        assert!(out.contains("\"a\": 1"), "got {out}");
    }

    #[test]
    fn body_for_log_summarises_binary_with_blake3() {
        let out = body_for_log(&[0u8, 159, 146, 150], Some("application/octet-stream"));
        assert!(out.starts_with("<4 bytes, blake3="), "got {out}");
    }

    #[test]
    fn body_for_log_reports_empty() {
        assert_eq!(body_for_log(&[], None), "<empty body>");
    }

    #[test]
    fn body_for_log_caps_long_text_without_scanning_whole_blob() {
        let body = vec![b'a'; TRACE_BODY_CAP * 4];
        let out = body_for_log(&body, Some("text/plain"));
        assert!(
            out.contains("bytes total>"),
            "got tail {}",
            &out[out.len() - 40..]
        );
        assert!(out.len() < body.len());
    }

    #[test]
    fn body_for_log_handles_multibyte_head_boundary() {
        let mut body = vec![b'a'; TRACE_BODY_CAP - 1];
        body.extend_from_slice("\u{1F4A9}".as_bytes());
        body.extend_from_slice(&[b'b'; 32]);
        let out = body_for_log(&body, Some("text/plain"));
        assert!(out.contains("bytes total>"), "got {out}");
    }

    #[test]
    fn trace_emit_paths_do_not_panic_at_max_verbosity() {
        let logger = Logger::from_flags(false, 9);
        logger.trace_http(&HttpCall {
            proto: "JMAP",
            method: "POST",
            url: "https://mail.example.com/jmap",
            status: 200,
            elapsed: Duration::from_millis(3),
            note: Some("Mailbox/get"),
            request: Some(br#"{"using":["urn:ietf:params:jmap:core"]}"#),
            request_type: Some("application/json"),
            response: &[0u8, 200, 1, 159, 146, 150],
            response_type: Some("application/octet-stream"),
        });
        logger.trace_cmd(
            "IMAP",
            "LOGIN \"alice\" \"s3cret\"",
            "OK done",
            Duration::from_millis(1),
        );
        logger.trace_http_error(
            "Graph",
            "GET",
            "https://graph.microsoft.com/v1.0/me",
            "host not found",
            Duration::from_millis(9),
        );
    }
}
