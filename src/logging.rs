/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;
use std::time::Duration;

use crate::console::{Console, Severity, Tone};

const HTTP_ERROR_STATUS: u16 = 400;

pub const TRACE_BODY_CAP: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Quiet,
    Default,
    Progress,
    Method,
    Bodies,
}

const LEVELS: [Level; 5] = [
    Level::Quiet,
    Level::Default,
    Level::Progress,
    Level::Method,
    Level::Bodies,
];

impl Level {
    pub fn from_index(index: u8) -> Level {
        LEVELS
            .get(usize::from(index))
            .copied()
            .unwrap_or(Level::Bodies)
    }

    pub fn index(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Verbose(u8),
}

#[derive(Debug, Clone, Copy)]
pub struct Logger {
    level: Level,
    console: Console,
}

impl Logger {
    pub fn new(level: Level) -> Self {
        Logger {
            level,
            console: Console::default(),
        }
    }

    pub fn with_console(self, console: Console) -> Self {
        Logger { console, ..self }
    }

    pub fn console(self) -> Console {
        self.console
    }

    pub fn from_verbosity(verbosity: Verbosity) -> Self {
        match verbosity {
            Verbosity::Quiet => Logger::new(Level::Quiet),
            Verbosity::Verbose(extra) => Logger::new(Level::from_index(
                Level::Default.index().saturating_add(extra),
            )),
        }
    }

    pub fn level(self) -> Level {
        self.level
    }

    pub fn enabled(self, level: Level) -> bool {
        self.level >= level
    }

    pub fn info(self, args: fmt::Arguments<'_>) {
        if self.enabled(Level::Default) {
            eprintln!("{args}");
        }
    }

    pub fn progress(self, args: fmt::Arguments<'_>) {
        if self.enabled(Level::Progress) {
            eprintln!("{args}");
        }
    }

    pub fn note(self, args: fmt::Arguments<'_>) {
        if self.enabled(Level::Default) {
            eprintln!("{} {args}", self.console.err.severity(Severity::Note));
        }
    }

    pub fn notice(self, args: fmt::Arguments<'_>) {
        eprintln!("{args}");
    }

    pub fn warn(self, args: fmt::Arguments<'_>) {
        eprintln!("{} {args}", self.console.err.severity(Severity::Warning));
    }

    pub fn error(self, args: fmt::Arguments<'_>) {
        eprintln!("{} {args}", self.console.err.severity(Severity::Error));
    }

    pub fn trace_http(self, call: &HttpCall<'_>) {
        if !self.enabled(Level::Method) {
            return;
        }
        let styled = self.console.err;
        let status_tone = if call.status < HTTP_ERROR_STATUS {
            Tone::Good
        } else {
            Tone::Bad
        };
        eprintln!(
            "{} {} {} {} {} {}{}",
            styled.paint(Tone::Heading, call.proto),
            call.method,
            call.url,
            styled.paint(Tone::Dim, "->"),
            styled.paint(status_tone, call.status),
            styled.paint(Tone::Dim, Millis(call.elapsed)),
            Note(call.note),
        );
        if self.enabled(Level::Bodies) {
            if let Some(body) = call.request {
                WireBody {
                    body,
                    content_type: call.request_type,
                }
                .trace('>');
            }
            WireBody {
                body: call.response,
                content_type: call.response_type,
            }
            .trace('<');
        }
    }

    pub fn trace_cmd(self, proto: &str, command: &str, status: &str, elapsed: Duration) {
        if !self.enabled(Level::Method) {
            return;
        }
        let redacted = WireCommand(command).to_string();
        let command = Capped {
            text: &redacted,
            total_bytes: command.len(),
        };
        let styled = self.console.err;
        eprintln!(
            "{} {command} {} {status} {}",
            styled.paint(Tone::Heading, proto),
            styled.paint(Tone::Dim, "->"),
            styled.paint(Tone::Dim, Millis(elapsed)),
        );
    }

    pub fn trace_http_error(
        self,
        proto: &str,
        method: &str,
        url: &str,
        error: &str,
        elapsed: Duration,
    ) {
        if !self.enabled(Level::Method) {
            return;
        }
        let styled = self.console.err;
        eprintln!(
            "{} {method} {url} {} {} {}",
            styled.paint(Tone::Heading, proto),
            styled.paint(Tone::Dim, "->"),
            styled.paint(Tone::Bad, format_args!("transport error: {error}")),
            styled.paint(Tone::Dim, Millis(elapsed)),
        );
    }
}

struct Millis(Duration);

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({} ms)", self.0.as_millis())
    }
}

struct Note<'a>(Option<&'a str>);

impl fmt::Display for Note<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(note) => write!(f, " {note}"),
            None => Ok(()),
        }
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

#[derive(Clone, Copy)]
pub struct WireBody<'a> {
    pub body: &'a [u8],
    pub content_type: Option<&'a str>,
}

impl<'a> WireBody<'a> {
    fn trace(self, marker: char) {
        for line in self.to_string().lines() {
            eprintln!("  {marker} {line}");
        }
    }

    fn is_json(self) -> bool {
        self.content_type.is_some_and(|c| {
            c.as_bytes()
                .windows(JSON_MARKER.len())
                .any(|w| w.eq_ignore_ascii_case(JSON_MARKER))
        })
    }

    fn head(self) -> &'a [u8] {
        let Some(prefix) = self.body.get(..=TRACE_BODY_CAP) else {
            return self.body;
        };
        prefix
            .iter()
            .rposition(|b| b & UTF8_CONTINUATION_MASK != UTF8_CONTINUATION)
            .and_then(|end| self.body.get(..end))
            .unwrap_or_default()
    }
}

const JSON_MARKER: &[u8] = b"json";
const UTF8_CONTINUATION_MASK: u8 = 0xC0;
const UTF8_CONTINUATION: u8 = 0x80;

impl fmt::Display for WireBody<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let body = self.body;
        if body.is_empty() {
            return f.write_str("<empty body>");
        }
        if self.is_json()
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
            && let Ok(pretty) = serde_json::to_string_pretty(&value)
        {
            return Capped {
                text: &pretty,
                total_bytes: body.len(),
            }
            .fmt(f);
        }
        let head = self.head();
        match std::str::from_utf8(head) {
            Ok(text)
                if !text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
            {
                f.write_str(text)?;
                if body.len() > head.len() {
                    write!(f, "\n... <{} bytes total>", body.len())?;
                }
                Ok(())
            }
            _ => write!(
                f,
                "<{} bytes, blake3={}>",
                body.len(),
                blake3::hash(body).to_hex()
            ),
        }
    }
}

struct Capped<'a> {
    text: &'a str,
    total_bytes: usize,
}

impl fmt::Display for Capped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.text.len() <= TRACE_BODY_CAP {
            return f.write_str(self.text);
        }
        let end = self.text.floor_char_boundary(TRACE_BODY_CAP);
        f.write_str(self.text.get(..end).unwrap_or_default())?;
        write!(f, "\n... <{} bytes total>", self.total_bytes)
    }
}

#[derive(Clone, Copy)]
enum Redact {
    Arguments,
    InitialResponse,
}

#[derive(Clone, Copy)]
pub struct WireCommand<'a>(pub &'a str);

impl fmt::Display for WireCommand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let trimmed = self.0.trim_end_matches(['\r', '\n']);
        let mut parts = trimmed.splitn(3, ' ');
        let first = parts.next().unwrap_or_default();
        let redact = hashify::map_ignore_case!(
            first.as_bytes(),
            Redact,
            "LOGIN" => Redact::Arguments,
            "AUTHENTICATE" => Redact::InitialResponse,
        );
        match redact {
            None => f.write_str(trimmed),
            Some(Redact::Arguments) => write!(f, "{first} <redacted>"),
            Some(Redact::InitialResponse) => {
                let mechanism = parts.next().unwrap_or_default();
                match parts.next() {
                    Some(_) => write!(f, "{first} {mechanism} <redacted>"),
                    None => f.write_str(trimmed.trim_end()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::Styled;

    #[test]
    fn quiet_is_level_zero() {
        assert_eq!(
            Logger::from_verbosity(Verbosity::Quiet).level(),
            Level::Quiet
        );
    }

    #[test]
    fn default_is_level_one() {
        assert_eq!(
            Logger::from_verbosity(Verbosity::Verbose(0)).level(),
            Level::Default
        );
    }

    #[test]
    fn verbose_caps_at_four() {
        assert_eq!(
            Logger::from_verbosity(Verbosity::Verbose(9)).level(),
            Level::Bodies
        );
    }

    #[test]
    fn enabled_is_a_superset_scale() {
        let l = Logger::from_verbosity(Verbosity::Verbose(1));
        assert!(l.enabled(Level::Default));
        assert!(l.enabled(Level::Progress));
        assert!(!l.enabled(Level::Method));
    }

    // The console is plain until set, and setting it keeps the level.
    #[test]
    fn with_console_keeps_the_level() {
        let logger = Logger::new(Level::Progress);
        assert_eq!(logger.console(), Console::default());
        let console = Console {
            out: Styled::Plain,
            err: Styled::Ansi,
        };
        let logger = logger.with_console(console);
        assert_eq!(logger.console(), console);
        assert_eq!(logger.level(), Level::Progress);
    }

    // Trace pieces render the unstyled text unchanged.
    #[test]
    fn trace_parts_keep_the_plain_text() {
        assert_eq!(Millis(Duration::from_millis(42)).to_string(), "(42 ms)");
        assert_eq!(Note(Some("retry 1")).to_string(), " retry 1");
        assert_eq!(Note(None).to_string(), "");
    }

    #[test]
    fn redact_wire_hides_login_arguments() {
        assert_eq!(
            WireCommand("LOGIN \"alice\" \"s3cret\"").to_string(),
            "LOGIN <redacted>"
        );
    }

    #[test]
    fn redact_wire_hides_authenticate_initial_response() {
        assert_eq!(
            WireCommand("AUTHENTICATE PLAIN AGFsaWNlAHMzY3JldA==").to_string(),
            "AUTHENTICATE PLAIN <redacted>"
        );
    }

    #[test]
    fn redact_wire_keeps_authenticate_without_initial_response() {
        assert_eq!(
            WireCommand("AUTHENTICATE PLAIN").to_string(),
            "AUTHENTICATE PLAIN"
        );
    }

    #[test]
    fn redact_wire_passes_through_ordinary_commands() {
        assert_eq!(WireCommand("SELECT INBOX\r\n").to_string(), "SELECT INBOX");
    }

    #[test]
    fn body_for_log_pretty_prints_json() {
        let out = WireBody {
            body: br#"{"a":1}"#,
            content_type: Some("application/json"),
        }
        .to_string();
        assert!(out.contains("\"a\": 1"), "got {out}");
    }

    #[test]
    fn body_for_log_summarises_binary_with_blake3() {
        let out = WireBody {
            body: &[0u8, 159, 146, 150],
            content_type: Some("application/octet-stream"),
        }
        .to_string();
        assert!(out.starts_with("<4 bytes, blake3="), "got {out}");
    }

    #[test]
    fn body_for_log_reports_empty() {
        assert_eq!(
            WireBody {
                body: &[],
                content_type: None
            }
            .to_string(),
            "<empty body>"
        );
    }

    #[test]
    fn body_for_log_caps_long_text_without_scanning_whole_blob() {
        let body = vec![b'a'; TRACE_BODY_CAP * 4];
        let out = WireBody {
            body: &body,
            content_type: Some("text/plain"),
        }
        .to_string();
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
        let out = WireBody {
            body: &body,
            content_type: Some("text/plain"),
        }
        .to_string();
        assert!(out.contains("bytes total>"), "got {out}");
    }

    #[test]
    fn trace_emit_paths_do_not_panic_at_max_verbosity() {
        let logger = Logger::from_verbosity(Verbosity::Verbose(9));
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
