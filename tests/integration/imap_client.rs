/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use super::data::MboxMessage;
use super::error::{ContainerError, ContainerResult};
use super::layouts::MailboxSpec;
use super::{Account, Endpoint};

pub struct ImapSeed {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    tag_seq: u32,
    pub separator: char,
}

impl ImapSeed {
    pub fn connect(host: &str, port: u16) -> ContainerResult<Self> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let stream = loop {
            match TcpStream::connect((host, port)) {
                Ok(s) => break s,
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(e.into());
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let writer = stream.try_clone()?;
        let mut me = Self {
            reader: BufReader::new(stream),
            writer,
            tag_seq: 0,
            separator: '/',
        };
        let greeting = me.read_line()?;
        if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
            return Err(ContainerError::Protocol(format!(
                "imap greeting unexpected: {greeting}"
            )));
        }
        Ok(me)
    }

    fn next_tag(&mut self) -> String {
        self.tag_seq += 1;
        format!("a{:04}", self.tag_seq)
    }

    fn read_line(&mut self) -> ContainerResult<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(ContainerError::Protocol("imap eof".to_owned()));
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(line)
    }

    fn read_tagged(&mut self, tag: &str) -> ContainerResult<Vec<String>> {
        let mut untagged = Vec::new();
        loop {
            let line = self.read_line()?;
            if let Some(rest) = line.strip_prefix(&format!("{tag} ")) {
                if rest.starts_with("OK") {
                    return Ok(untagged);
                }
                return Err(ContainerError::Protocol(format!(
                    "imap tagged failure: {line}"
                )));
            }
            untagged.push(line);
        }
    }

    fn send(&mut self, line: &str) -> ContainerResult<()> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn login(&mut self, user: &str, password: &str) -> ContainerResult<()> {
        let tag = self.next_tag();
        let escaped_user = quote(user);
        let escaped_pw = quote(password);
        self.send(&format!("{tag} LOGIN {escaped_user} {escaped_pw}"))?;
        self.read_tagged(&tag)?;
        Ok(())
    }

    pub fn discover_separator(&mut self) -> ContainerResult<char> {
        let tag = self.next_tag();
        self.send(&format!(r#"{tag} LIST "" """#))?;
        let untagged = self.read_tagged(&tag)?;
        for line in &untagged {
            if let Some(rest) = line.strip_prefix("* LIST ")
                && let Some(open) = rest.find('"')
                && let Some(close) = rest[open + 1..].find('"')
            {
                let sep = &rest[open + 1..open + 1 + close];
                if let Some(c) = sep.chars().next() {
                    self.separator = c;
                    return Ok(c);
                }
            }
        }
        Ok('/')
    }

    pub fn create(&mut self, full_path: &str) -> ContainerResult<()> {
        let tag = self.next_tag();
        self.send(&format!("{tag} CREATE {}", quote(full_path)))?;
        match self.read_tagged(&tag) {
            Ok(_) => Ok(()),
            Err(ContainerError::Protocol(msg)) if msg.contains("already exists") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn subscribe(&mut self, full_path: &str) -> ContainerResult<()> {
        let tag = self.next_tag();
        self.send(&format!("{tag} SUBSCRIBE {}", quote(full_path)))?;
        let _ = self.read_tagged(&tag);
        Ok(())
    }

    pub fn append(&mut self, mailbox: &str, message: &[u8]) -> ContainerResult<()> {
        self.append_with_flags(mailbox, &[], message)
    }

    pub fn append_with_flags(
        &mut self,
        mailbox: &str,
        flags: &[&str],
        message: &[u8],
    ) -> ContainerResult<()> {
        let tag = self.next_tag();
        let flag_clause = if flags.is_empty() {
            String::new()
        } else {
            format!(" ({})", flags.join(" "))
        };
        let cmd = format!(
            "{tag} APPEND {}{flag_clause} {{{}}}\r\n",
            quote(mailbox),
            message.len(),
        );
        self.writer.write_all(cmd.as_bytes())?;
        self.writer.flush()?;
        let cont = self.read_line()?;
        if !cont.starts_with('+') {
            return Err(ContainerError::Protocol(format!(
                "append expected continuation, got: {cont}"
            )));
        }
        self.writer.write_all(message)?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()?;
        self.read_tagged(&tag)?;
        Ok(())
    }

    pub fn list_all(&mut self) -> ContainerResult<Vec<String>> {
        let tag = self.next_tag();
        self.send(&format!(r#"{tag} LIST "" "*""#))?;
        let untagged = self.read_tagged(&tag)?;
        let mut names = Vec::new();
        for line in untagged {
            if let Some(rest) = line.strip_prefix("* LIST ")
                && let Some(name) = parse_list_name(rest)
            {
                names.push(name);
            }
        }
        Ok(names)
    }

    pub fn list_raw(&mut self) -> ContainerResult<Vec<String>> {
        let tag = self.next_tag();
        self.send(&format!(r#"{tag} LIST "" "*""#))?;
        self.read_tagged(&tag)
    }

    pub fn select(&mut self, mailbox: &str) -> ContainerResult<usize> {
        let tag = self.next_tag();
        self.send(&format!("{tag} SELECT {}", quote(mailbox)))?;
        let untagged = self.read_tagged(&tag)?;
        for line in untagged {
            if let Some(rest) = line.strip_prefix("* ")
                && let Some(num) = rest.strip_suffix(" EXISTS")
                && let Ok(n) = num.parse::<usize>()
            {
                return Ok(n);
            }
        }
        Ok(0)
    }

    pub fn delete_and_expunge_first(&mut self, mailbox: &str) -> ContainerResult<()> {
        self.select(mailbox)?;
        let store_tag = self.next_tag();
        self.send(&format!("{store_tag} STORE 1 +FLAGS (\\Deleted)"))?;
        self.read_tagged(&store_tag)?;
        let expunge_tag = self.next_tag();
        self.send(&format!("{expunge_tag} EXPUNGE"))?;
        self.read_tagged(&expunge_tag)?;
        Ok(())
    }

    pub fn logout(&mut self) -> ContainerResult<()> {
        let tag = self.next_tag();
        self.send(&format!("{tag} LOGOUT"))?;
        let mut buf = String::new();
        let _ = self.reader.read_to_string(&mut buf);
        Ok(())
    }

    pub fn open(imap: &Endpoint, account: &Account) -> ContainerResult<Self> {
        let mut client = Self::connect(&imap.host, imap.port)?;
        client.login(&account.username, &account.password)?;
        Ok(client)
    }

    pub fn seed_layout(
        &mut self,
        account: &Account,
        messages: &[MboxMessage],
        probes: &SeedProbes,
    ) -> ContainerResult<MailboxSeed> {
        let sep = self.discover_separator().unwrap_or('/');

        let mut paths = Vec::new();
        for spec in account.layout.mailboxes {
            let path = full_path(account.layout.mailboxes, spec.key, sep).ok_or_else(|| {
                ContainerError::Seed(format!("missing mailbox key: {}", spec.key))
            })?;
            self.create(&path)?;
            self.subscribe(&path)?;
            paths.push(path);
        }

        let mut targets: Vec<String> = vec!["INBOX".to_owned()];
        targets.extend(paths.iter().cloned());

        let total = account.layout.email_count.min(messages.len());
        if total < account.layout.email_count {
            return Err(ContainerError::Seed(format!(
                "{} only has {} messages but layout requires {}",
                account.username,
                messages.len(),
                account.layout.email_count
            )));
        }

        let mut seed = MailboxSeed {
            histogram: targets.iter().map(|t| (t.clone(), 0)).collect(),
            paths,
            total_appends: 0,
            extras: ExtraAppends::default(),
            appends: Vec::new(),
        };

        for ((i, msg), target) in messages
            .iter()
            .take(total)
            .enumerate()
            .zip(targets.iter().cycle())
        {
            self.append_with_flags(target, &[], &msg.raw)?;
            seed.record(target, msg.raw.clone(), Vec::new(), SeedTag::Bulk(i));
        }

        if let (Some(target), Some(first)) = (targets.get(1), messages.first()) {
            self.append_with_flags(target, &[], &first.raw)?;
            seed.record(target, first.raw.clone(), Vec::new(), SeedTag::Dedup);
            seed.extras.dedup_target = Some(target.clone());
        }

        if !messages.is_empty() {
            self.append_with_flags("INBOX", &["\\Seen", "\\Flagged"], &probes.flag)?;
            seed.record(
                "INBOX",
                probes.flag.clone(),
                vec!["$seen".to_owned(), "$flagged".to_owned()],
                SeedTag::FlagProbe,
            );
            seed.extras.flagged_target = Some("INBOX".to_owned());
        }

        if let Some(probe) = &probes.no_message_id
            && !messages.is_empty()
        {
            self.append_with_flags("INBOX", &[], probe)?;
            seed.record("INBOX", probe.clone(), Vec::new(), SeedTag::NoMessageId);
            seed.extras.nomid_target = Some("INBOX".to_owned());
        }

        if let (Some(body), [_, a, b, ..]) = (&probes.shared_message_id, targets.as_slice()) {
            self.append_with_flags(a, &[], body)?;
            self.append_with_flags(b, &[], body)?;
            seed.record(a, body.clone(), Vec::new(), SeedTag::SharedMid);
            seed.record(b, body.clone(), Vec::new(), SeedTag::SharedMid);
            seed.extras.mid_dedup_targets = Some((a.clone(), b.clone()));
        }

        Ok(seed)
    }

    pub fn verify_layout(
        &mut self,
        account: &Account,
        seed: &MailboxSeed,
        who: &str,
    ) -> ContainerResult<()> {
        let names = self.list_all()?;
        let expected = account.layout.mailboxes.len() + 1;
        if names.len() < expected {
            return Err(ContainerError::Seed(format!(
                "{who}: expected >= {expected} mailboxes, got {}",
                names.len()
            )));
        }
        let inbox_n = self.select("INBOX")?;
        if account.layout.email_count > 0 && inbox_n == 0 {
            return Err(ContainerError::Seed(format!(
                "{who}: INBOX EXISTS = 0 after seed"
            )));
        }
        for path in &seed.paths {
            let n = self.select(path)?;
            let want = seed.histogram.get(path).copied().unwrap_or(0);
            if n < want {
                return Err(ContainerError::Seed(format!(
                    "{who}: mailbox {path} EXISTS={n} but {want} were appended"
                )));
            }
        }
        Ok(())
    }

    pub fn append_probe(
        &mut self,
        account: &Account,
        mailbox: &str,
        tag: &str,
    ) -> ContainerResult<(Vec<u8>, String)> {
        let message_id = format!("<added-{tag}-{}@vandelay.test>", account.username);
        let body = format!(
            "From: added-{tag}@vandelay.test\r\n\
             To: {}@vandelay.test\r\n\
             Subject: Added probe {tag}\r\n\
             Message-ID: {message_id}\r\n\
             Date: Wed, 01 Jan 2025 12:00:00 +0000\r\n\
             \r\n\
             Added probe body {tag}.\r\n",
            account.username
        );
        let raw = body.into_bytes();
        self.append_with_flags(mailbox, &[], &raw)?;
        Ok((raw, message_id))
    }

    pub fn create_with_message(&mut self, wire_name: &str, message: &[u8]) -> ContainerResult<()> {
        self.create(wire_name)?;
        self.subscribe(wire_name)?;
        self.append_with_flags(wire_name, &[], message)
    }
}

pub struct SeedProbes {
    pub flag: Vec<u8>,
    pub no_message_id: Option<Vec<u8>>,
    pub shared_message_id: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct MailboxSeed {
    pub paths: Vec<String>,
    pub histogram: HashMap<String, usize>,
    pub total_appends: usize,
    pub extras: ExtraAppends,
    pub appends: Vec<SeededAppend>,
}

impl MailboxSeed {
    fn record(&mut self, target: &str, raw: Vec<u8>, flags: Vec<String>, tag: SeedTag) {
        *self.histogram.entry(target.to_owned()).or_insert(0) += 1;
        self.total_appends += 1;
        self.appends.push(SeededAppend {
            raw,
            target: target.to_owned(),
            flags,
            tag,
        });
    }
}

#[derive(Debug, Clone)]
pub struct SeededAppend {
    pub raw: Vec<u8>,
    pub target: String,
    pub flags: Vec<String>,
    pub tag: SeedTag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedTag {
    Bulk(usize),
    Dedup,
    FlagProbe,
    NoMessageId,
    SharedMid,
}

#[derive(Debug, Clone, Default)]
pub struct ExtraAppends {
    pub dedup_target: Option<String>,
    pub flagged_target: Option<String>,
    pub nomid_target: Option<String>,
    pub mid_dedup_targets: Option<(String, String)>,
}

pub fn full_path(specs: &[MailboxSpec], key: &str, separator: char) -> Option<String> {
    let mut chain: Vec<&str> = Vec::new();
    let mut cur = key;
    loop {
        let spec = specs.iter().find(|s| s.key == cur)?;
        chain.push(spec.name);
        match spec.parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    chain.reverse();
    Some(chain.join(&separator.to_string()))
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn parse_list_name(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let _ = chars.next()?;
    let mut depth = 1;
    let mut idx = 1;
    for (i, c) in line[1..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    idx = i + 2;
                    break;
                }
            }
            _ => {}
        }
    }
    let rest = line[idx..].trim_start();
    let mut parts = rest.splitn(2, ' ');
    let _sep_token = parts.next()?;
    let name_token = parts.next()?.trim();
    if let Some(stripped) = name_token.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_owned())
    } else {
        Some(name_token.to_owned())
    }
}
