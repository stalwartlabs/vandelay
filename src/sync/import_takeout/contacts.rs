/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::path::Path;

use rusqlite::{Connection, Transaction, params};

use crate::db::takeout_ids;
use crate::error::Error;
use crate::logging::Logger;
use crate::sync::TypeCounts;
use crate::sync::import_dav::calcard;

const DEFAULT_ADDRESS_BOOK: &str = "Imported";

pub fn process_file(
    conn: &mut Connection,
    file_path: &Path,
    source_id: i64,
    address_book_local: &mut Option<i64>,
    book_counts: &mut TypeCounts,
    card_counts: &mut TypeCounts,
    logger: Logger,
) -> Result<(), Error> {
    let text = match std::fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => {
            logger.warn(&format!("{file_path:?}: read: {e}"));
            card_counts.failed += 1;
            return Ok(());
        }
    };

    let chunks = split_vcards(&text);
    if chunks.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction()?;
    let book_local = match *address_book_local {
        Some(id) => id,
        None => {
            let id = ensure_default_address_book(&tx, source_id, book_counts)?;
            *address_book_local = Some(id);
            id
        }
    };

    for (ordinal, chunk) in chunks.into_iter().enumerate() {
        match insert_card(&tx, source_id, book_local, &chunk, card_counts) {
            Ok(()) => {}
            Err(e) => {
                logger.warn(&format!("{file_path:?}#{ordinal}: vCard: {e}"));
                card_counts.failed += 1;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

fn ensure_default_address_book(
    tx: &Transaction<'_>,
    source_id: i64,
    counts: &mut TypeCounts,
) -> Result<i64, Error> {
    if let Some(id) = takeout_ids::local_for(
        tx,
        source_id,
        takeout_ids::ADDRESS_BOOK,
        DEFAULT_ADDRESS_BOOK,
    )? {
        return Ok(id);
    }
    tx.execute(
        "INSERT INTO address_books
            (name, description, sort_order, is_default, is_subscribed)
         VALUES (?1, NULL, 0, 0, 1)",
        params![DEFAULT_ADDRESS_BOOK],
    )?;
    let id = tx.last_insert_rowid();
    takeout_ids::insert(
        tx,
        source_id,
        takeout_ids::ADDRESS_BOOK,
        DEFAULT_ADDRESS_BOOK,
        id,
    )?;
    counts.created += 1;
    counts.fetched += 1;
    Ok(id)
}

fn insert_card(
    tx: &Transaction<'_>,
    source_id: i64,
    book_local: i64,
    chunk: &str,
    counts: &mut TypeCounts,
) -> Result<(), Error> {
    let card = calcard::vcard_to_jscontact(chunk, chunk)
        .map_err(|e| Error::Partial(format!("vCard parse: {e}")))?;
    let data_json = card.data.to_string();
    let address_book_ids = format!("[{book_local}]");

    let existing = takeout_ids::local_for(tx, source_id, takeout_ids::CONTACT_CARD, &card.uid)?;

    if let Some(local) = existing {
        let current: (String, String, String) = tx.query_row(
            "SELECT uid, address_book_ids, data FROM contact_cards WHERE id = ?1",
            params![local],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let want = (
            card.uid.clone(),
            address_book_ids.clone(),
            data_json.clone(),
        );
        if current == want {
            counts.skipped += 1;
            return Ok(());
        }
        tx.execute(
            "UPDATE contact_cards SET uid = ?1, address_book_ids = ?2, data = ?3
             WHERE id = ?4",
            params![card.uid, address_book_ids, data_json, local],
        )?;
        counts.fetched += 1;
        return Ok(());
    }

    tx.execute(
        "INSERT INTO contact_cards (uid, address_book_ids, data) VALUES (?1, ?2, ?3)",
        params![card.uid, address_book_ids, data_json],
    )?;
    let local_id = tx.last_insert_rowid();
    takeout_ids::insert(
        tx,
        source_id,
        takeout_ids::CONTACT_CARD,
        &card.uid,
        local_id,
    )?;
    counts.created += 1;
    counts.fetched += 1;
    Ok(())
}

fn split_vcards(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        let bytes = line.as_bytes();
        if starts_with_ignore_ascii_case(bytes, b"BEGIN:VCARD") {
            current = Some(String::new());
        }
        if let Some(buf) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
        if starts_with_ignore_ascii_case(bytes, b"END:VCARD")
            && let Some(buf) = current.take()
        {
            chunks.push(buf);
        }
    }
    chunks
}

fn starts_with_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_single_card() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nEND:VCARD\r\n";
        let chunks = split_vcards(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("FN:A"));
    }

    #[test]
    fn splits_multiple_cards() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\nEND:VCARD\r\n\
            BEGIN:VCARD\r\nVERSION:3.0\r\nFN:B\r\nEND:VCARD\r\n\
            BEGIN:VCARD\r\nVERSION:3.0\r\nFN:C\r\nEND:VCARD\r\n";
        let chunks = split_vcards(text);
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
        let chunks = split_vcards(text);
        assert_eq!(chunks.len(), 2);
        assert!(!chunks[0].contains("garbage"));
    }

    #[test]
    fn preserves_folded_continuation_lines_inside_a_card() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\n\
            ADR:;;1 Infinite Loop\\nCupertino\\, CA 95014\\nUnited States\r\n \
            ;;;;;\r\nEND:VCARD\r\n";
        let chunks = split_vcards(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("ADR:"));
        assert!(chunks[0].contains(";;;;;"));
    }

    #[test]
    fn handles_lf_only_line_endings() {
        let text = "BEGIN:VCARD\nVERSION:3.0\nFN:LF\nEND:VCARD\n";
        let chunks = split_vcards(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("FN:LF"));
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(split_vcards("").is_empty());
        assert!(split_vcards("not a vcard\n").is_empty());
    }

    #[test]
    fn unterminated_card_is_dropped() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:A\r\n";
        let chunks = split_vcards(text);
        assert!(chunks.is_empty());
    }

    #[test]
    fn case_insensitive_boundary_match() {
        let text = "begin:vcard\r\nFN:CaseTest\r\nend:vcard\r\n";
        let chunks = split_vcards(text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("FN:CaseTest"));
    }
}
