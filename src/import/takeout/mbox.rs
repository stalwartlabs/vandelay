/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::str;

use time::{Date, Month, OffsetDateTime, Time, UtcOffset};

use crate::error::Error;
use crate::sync::dates::{MissingDate, Rfc3339};

pub struct MessageIterator<T> {
    reader: T,
    message: Option<Message>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Message {
    internal_date: u64,
    contents: Vec<u8>,
}

impl<T> MessageIterator<T>
where
    T: BufRead,
{
    pub fn new(reader: T) -> MessageIterator<T> {
        MessageIterator {
            reader,
            message: None,
        }
    }
}

impl MessageIterator<BufReader<File>> {
    pub fn open(path: &Path) -> Result<Self, Error> {
        File::open(path)
            .map(|file| MessageIterator::new(BufReader::new(file)))
            .map_err(|e| Error::Partial(format!("open {path:?}: {e}")))
    }
}

impl<T> Iterator for MessageIterator<T>
where
    T: BufRead,
{
    type Item = io::Result<Message>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = Vec::with_capacity(80);

        loop {
            match self.reader.read_until(b'\n', &mut line) {
                Ok(0) => return self.message.take().map(Ok),
                Ok(_) => {}
                Err(e) => return Some(Err(e)),
            }

            if let Some(envelope) = line.strip_prefix(b"From ") {
                let finished = self.message.take().map(Ok);
                self.message = Some(Message::from_envelope(envelope));
                if finished.is_some() {
                    return finished;
                }
                line.clear();
                continue;
            }

            if let Some(message) = &mut self.message {
                match line.split_first() {
                    Some((b'>', unquoted)) if Message::is_quoted_from(unquoted) => {
                        message.contents.extend_from_slice(unquoted);
                    }
                    _ => message.contents.extend_from_slice(&line),
                }
            }
            line.clear();
        }
    }
}

impl Message {
    fn from_envelope(envelope: &[u8]) -> Self {
        let envelope = str::from_utf8(envelope).unwrap_or("");
        let date = envelope.split_once(' ').map_or("", |(_, rest)| rest);
        Self {
            internal_date: EnvelopeDate::parse(date).unwrap_or(0),
            contents: Vec::with_capacity(1024),
        }
    }

    fn is_quoted_from(mut line: &[u8]) -> bool {
        while let [b'>', rest @ ..] = line {
            line = rest;
        }
        line.starts_with(b"From ")
    }

    pub fn received_at(&self, date_header: Option<String>) -> String {
        if self.internal_date > 0 {
            return Rfc3339::from_unix(self.internal_date);
        }
        date_header.unwrap_or_else(|| MissingDate::Now.resolve())
    }

    pub fn contents(&self) -> &[u8] {
        &self.contents
    }
}

struct EnvelopeDate;

impl EnvelopeDate {
    fn parse(s: &str) -> Option<u64> {
        let mut parts = s.split_whitespace();
        let (month, day, time, offset, year) = match (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) {
            (Some(_), Some(month), Some(day), Some(time), Some(year), None, None) => {
                (month, day, time, None, year)
            }
            (Some(_), Some(month), Some(day), Some(time), Some(offset), Some(year), None) => {
                (month, day, time, Some(offset), year)
            }
            _ => return None,
        };

        let month = EnvelopeDate::month(month)?;
        let day: u8 = day.parse().ok()?;
        let (hour, minute, second) = EnvelopeDate::hms(time)?;
        let year: i32 = year.parse().ok()?;
        let offset = match offset {
            Some(offset) => EnvelopeDate::offset(offset)?,
            None => UtcOffset::UTC,
        };

        let date = Date::from_calendar_date(year, month, day).ok()?;
        let time = Time::from_hms(hour, minute, second).ok()?;
        let ts = OffsetDateTime::new_in_offset(date, time, offset).unix_timestamp();
        u64::try_from(ts).ok()
    }

    fn month(s: &str) -> Option<Month> {
        hashify::map_ignore_case!(s.as_bytes(), Month,
            "jan" => Month::January,
            "feb" => Month::February,
            "mar" => Month::March,
            "apr" => Month::April,
            "may" => Month::May,
            "jun" => Month::June,
            "jul" => Month::July,
            "aug" => Month::August,
            "sep" => Month::September,
            "oct" => Month::October,
            "nov" => Month::November,
            "dec" => Month::December,
        )
        .copied()
    }

    fn hms(s: &str) -> Option<(u8, u8, u8)> {
        let mut it = s.split(':');
        let h: u8 = it.next()?.parse().ok()?;
        let m: u8 = it.next()?.parse().ok()?;
        let sec: u8 = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some((h, m, sec))
    }

    fn offset(s: &str) -> Option<UtcOffset> {
        let (sign, digits) = match s.as_bytes() {
            [b'+', digits @ ..] => (1i8, digits),
            [b'-', digits @ ..] => (-1i8, digits),
            _ => return None,
        };
        let [h1, h2, m1, m2] = digits else {
            return None;
        };
        let hh: i8 = str::from_utf8(&[*h1, *h2]).ok()?.parse().ok()?;
        let mm: i8 = str::from_utf8(&[*m1, *m2]).ok()?.parse().ok()?;
        UtcOffset::from_hms(sign * hh, sign * mm, 0).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_ctime_envelope_parses() {
        let raw = b"From god@heaven.af.mil Sat Jan  3 01:05:34 1996\n\
            Message 1\n\
            \n\
            From cras@irccrew.org  Tue Jul 23 19:39:23 2002\n\
            Message 2\n\
            \n";
        let mut it = MessageIterator::new(&raw[..]);
        let m1 = it.next().unwrap().unwrap();
        assert_eq!(m1.internal_date, 820631134);
        assert_eq!(m1.contents(), b"Message 1\n\n");
        let m2 = it.next().unwrap().unwrap();
        assert_eq!(m2.internal_date, 1027453163);
        assert!(it.next().is_none());
    }

    #[test]
    fn mboxrd_unescape_recovers_lines() {
        let raw = b"From a@b Tue Aug  6 13:34:34 2002\n\
            Message 3\n\
            >From hello\n\
            >>From world\n\
            >>>From test\n\
            \n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        assert_eq!(
            m.contents(),
            b"Message 3\nFrom hello\n>From world\n>>From test\n\n"
        );
    }

    #[test]
    fn mboxrd_leaves_non_from_lines_starting_with_gt_alone() {
        let raw = b"From a@b Mon Jan 15  15:30:00  2018\n\
            Message 4\n\
            > From\n\
            >F\n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        assert_eq!(m.contents(), b"Message 4\n> From\n>F\n");
    }

    #[test]
    fn takeout_envelope_with_utc_offset_parses() {
        let raw = b"From 1848707889910060830@xxx Thu Nov 13 20:20:33 +0000 2025\n\
            X-GM-THRID: 1848707889910060830\n\
            X-Gmail-Labels: Inbox,Opened\n\
            Subject: hi\n\
            \n\
            body\n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        assert_eq!(m.internal_date, 1763065233);
        assert!(m.contents().starts_with(b"X-GM-THRID:"));
    }

    #[test]
    fn takeout_envelope_with_negative_offset_applies_offset() {
        let raw = b"From x@y Thu Nov 13 12:20:33 -0800 2025\n\
            body\n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        assert_eq!(m.internal_date, 1763065233);
    }

    #[test]
    fn malformed_envelope_date_yields_zero_not_error() {
        let raw = b"From who knows what\nbody\n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        assert_eq!(m.internal_date, 0);
    }

    #[test]
    fn streams_multiple_takeout_format_messages() {
        let raw = b"From 1@xxx Thu Nov 13 20:20:33 +0000 2025\n\
            X-GM-THRID: 1\n\
            X-Gmail-Labels: Inbox\n\
            Subject: A\n\
            \n\
            body a\n\
            From 2@xxx Thu Nov 13 21:00:00 +0000 2025\n\
            X-GM-THRID: 2\n\
            X-Gmail-Labels: Sent,Archived\n\
            Subject: B\n\
            \n\
            body b\n";
        let parsed: Vec<_> = MessageIterator::new(&raw[..])
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].contents().starts_with(b"X-GM-THRID: 1"));
    }

    #[test]
    fn received_at_prefers_envelope_internal_date() {
        let raw = b"From x@y Thu Nov 13 20:20:33 +0000 2025\nbody\n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        let got = m.received_at(Some("2025-05-12T10:00:00+02:00".to_owned()));
        assert_eq!(got, "2025-11-13T20:20:33Z");
    }

    #[test]
    fn received_at_falls_back_to_date_header() {
        let raw = b"From who knows what\nbody\n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        let got = m.received_at(Some("2025-05-12T10:00:00+02:00".to_owned()));
        assert_eq!(got, "2025-05-12T10:00:00+02:00");
    }

    #[test]
    fn received_at_last_resort_is_now_in_rfc3339() {
        let raw = b"From who knows what\nbody\n";
        let m = MessageIterator::new(&raw[..]).next().unwrap().unwrap();
        let got = m.received_at(None);
        assert!(got.ends_with("Z") || got.contains("+") || got.contains("-"));
        assert!(got.contains("T"));
    }
}
