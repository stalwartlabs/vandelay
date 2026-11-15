/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fs;
use std::path::Path;

use crate::archive::ids::takeout::TakeoutIds;
use crate::archive::rows::{AddressBookRecord, ContactCardRecord, Subscribed, Write};
use crate::archive::{LocalId, Store};
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::calcard::JsContact;
use crate::types::ObjectType;

pub const DEFAULT_ADDRESS_BOOK: &str = "Imported";

pub struct VCardFile;

impl VCardFile {
    pub fn load(path: &Path, card_counts: &mut TypeCounts, logger: Logger) -> Vec<String> {
        match fs::read_to_string(path) {
            Ok(text) => VCardFile::split(&text),
            Err(e) => {
                logger.warn(format_args!("cannot read {}: {e}", path.display()));
                card_counts.failed += 1;
                Vec::new()
            }
        }
    }

    fn split(text: &str) -> Vec<String> {
        let mut chunks: Vec<String> = Vec::new();
        let mut current: Option<String> = None;
        for raw_line in text.lines() {
            let line = raw_line.trim_end_matches('\r');
            if VCardFile::starts_with_ignore_case(line, "BEGIN:VCARD") {
                current = Some(String::new());
            }
            if let Some(buf) = current.as_mut() {
                buf.push_str(line);
                buf.push('\n');
            }
            if VCardFile::starts_with_ignore_case(line, "END:VCARD")
                && let Some(buf) = current.take()
            {
                chunks.push(buf);
            }
        }
        chunks
    }

    fn starts_with_ignore_case(line: &str, prefix: &str) -> bool {
        line.as_bytes()
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
    }
}

pub struct ContactWriter<'a> {
    pub store: Store<'a>,
    pub ids: TakeoutIds<'a>,
    pub book: &'a mut Option<LocalId>,
    pub book_counts: &'a mut TypeCounts,
    pub card_counts: &'a mut TypeCounts,
    pub logger: Logger,
}

impl ContactWriter<'_> {
    pub fn write(&mut self, path: &Path, chunks: &[String]) -> Result<(), Error> {
        let book = match *self.book {
            Some(id) => id,
            None => {
                let id = self.ensure_default_address_book()?;
                *self.book = Some(id);
                id
            }
        };
        for (ordinal, chunk) in chunks.iter().enumerate() {
            if let Err(e) = self.write_card(book, chunk) {
                self.logger.warn(format_args!(
                    "cannot store vCard {ordinal} of {}: {e}",
                    path.display()
                ));
                self.card_counts.failed += 1;
            }
        }
        Ok(())
    }

    fn ensure_default_address_book(&mut self) -> Result<LocalId, Error> {
        if let Some(id) = self
            .ids
            .local_id(ObjectType::AddressBook, DEFAULT_ADDRESS_BOOK)?
        {
            return Ok(id);
        }
        let id = self.store.address_books().insert(&AddressBookRecord {
            name: DEFAULT_ADDRESS_BOOK,
            description: None,
            sort_order: 0,
            is_default: false,
            subscribed: Subscribed::Yes,
        })?;
        self.ids
            .insert(ObjectType::AddressBook, DEFAULT_ADDRESS_BOOK, id)?;
        self.book_counts.created += 1;
        self.book_counts.fetched += 1;
        Ok(id)
    }

    fn write_card(&mut self, book: LocalId, chunk: &str) -> Result<(), Error> {
        let card = JsContact::from_vcard(chunk, chunk)
            .map_err(|e| Error::Partial(format!("vCard parse: {e}")))?;
        let data = card.data.to_string();
        let record = ContactCardRecord {
            uid: &card.uid,
            address_books: &[book],
            data: &data,
        };
        let cards = self.store.contact_cards();
        if let Some(local) = self.ids.local_id(ObjectType::ContactCard, &card.uid)? {
            match cards.update(local, &record)? {
                Write::Changed => self.card_counts.fetched += 1,
                Write::Unchanged => self.card_counts.skipped += 1,
            }
            return Ok(());
        }
        let local = cards.insert(&record)?;
        self.ids.insert(ObjectType::ContactCard, &card.uid, local)?;
        self.card_counts.created += 1;
        self.card_counts.fetched += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_single_card() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nEND:VCARD\r\n";
        let chunks = VCardFile::split(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("FN:A"));
    }

    #[test]
    fn splits_multiple_cards() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nEND:VCARD\r\n\
            BEGIN:VCARD\r\nVERSION:3.0\r\nFN:B\r\nEND:VCARD\r\n\
            BEGIN:VCARD\r\nVERSION:3.0\r\nFN:C\r\nEND:VCARD\r\n";
        let chunks = VCardFile::split(text);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("FN:A"));
        assert!(chunks[1].contains("FN:B"));
        assert!(chunks[2].contains("FN:C"));
    }

    #[test]
    fn skips_lines_outside_any_card() {
        let text = "Some preamble garbage\r\n\
            BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nEND:VCARD\r\n\
            garbage between cards\r\n\
            BEGIN:VCARD\r\nVERSION:3.0\r\nFN:B\r\nEND:VCARD\r\n\
            trailing garbage\r\n";
        let chunks = VCardFile::split(text);
        assert_eq!(chunks.len(), 2);
        assert!(!chunks[0].contains("garbage"));
    }

    #[test]
    fn preserves_folded_continuation_lines_inside_a_card() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\n\
            ADR:;;1 Infinite Loop\\nCupertino\\, CA 95014\\nUnited States\r\n \
            ;;;;;\r\nEND:VCARD\r\n";
        let chunks = VCardFile::split(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("ADR:"));
        assert!(chunks[0].contains(";;;;;"));
    }

    #[test]
    fn handles_lf_only_line_endings() {
        let text = "BEGIN:VCARD\nVERSION:3.0\nFN:LF\nEND:VCARD\n";
        let chunks = VCardFile::split(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("FN:LF"));
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(VCardFile::split("").is_empty());
        assert!(VCardFile::split("not a vcard\n").is_empty());
    }

    #[test]
    fn unterminated_card_is_dropped() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\n";
        let chunks = VCardFile::split(text);
        assert!(chunks.is_empty());
    }

    #[test]
    fn case_insensitive_boundary_match() {
        let text = "begin:vcard\r\nFN:CaseTest\r\nend:vcard\r\n";
        let chunks = VCardFile::split(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("FN:CaseTest"));
    }
}
