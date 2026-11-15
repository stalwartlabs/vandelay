/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use crate::imap::response::{Untagged, Value};

#[derive(Debug, Default, Clone)]
pub struct FetchAttrs {
    pub uid: Option<u32>,
    pub flags: Vec<String>,
    pub internaldate: Option<String>,
    pub size: Option<u64>,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
enum FetchItem {
    Uid,
    Flags,
    InternalDate,
    Size,
    Body,
}

impl FetchAttrs {
    pub fn from_untagged(u: Untagged) -> Option<FetchAttrs> {
        let Untagged::Fetch { items, .. } = u else {
            return None;
        };
        let mut out = FetchAttrs::default();
        for (name, value) in items {
            let Some(item) = hashify::map!(name.as_bytes(), FetchItem,
                "UID" => FetchItem::Uid,
                "FLAGS" => FetchItem::Flags,
                "INTERNALDATE" => FetchItem::InternalDate,
                "RFC822.SIZE" => FetchItem::Size,
                "BODY[]" => FetchItem::Body,
                "RFC822" => FetchItem::Body,
            ) else {
                continue;
            };
            match (item, value) {
                (FetchItem::Uid, Value::Number(n)) => out.uid = u32::try_from(n).ok(),
                (FetchItem::Flags, Value::List(items)) => {
                    out.flags = items
                        .into_iter()
                        .filter_map(|v| match v {
                            Value::Atom(s) | Value::Str(s) => Some(s),
                            _ => None,
                        })
                        .collect();
                }
                (FetchItem::InternalDate, Value::Atom(s) | Value::Str(s)) => {
                    out.internaldate = Some(s);
                }
                (FetchItem::Size, Value::Number(n)) => out.size = Some(n),
                (FetchItem::Body, Value::Bytes(b)) => out.body = Some(b),
                (FetchItem::Body, Value::Str(s)) => out.body = Some(s.into_bytes()),
                (FetchItem::Body, _) => out.body = None,
                _ => {}
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_fetch(input: &[u8]) -> FetchAttrs {
        let resp = crate::imap::response::Response::read(&mut Cursor::new(input)).unwrap();
        match resp {
            crate::imap::response::Response::Untagged(u) => {
                FetchAttrs::from_untagged(u).expect("fetch attrs")
            }
            _ => panic!("expected untagged"),
        }
    }

    #[test]
    fn extracts_uid_flags_size_internaldate() {
        let f = parse_fetch(
            b"* 1 FETCH (UID 42 FLAGS (\\Seen) INTERNALDATE \"01-Jan-2024 12:00:00 +0000\" RFC822.SIZE 4242)\r\n",
        );
        assert_eq!(f.uid, Some(42));
        assert_eq!(f.flags, vec!["\\Seen"]);
        assert_eq!(
            f.internaldate.as_deref(),
            Some("01-Jan-2024 12:00:00 +0000")
        );
        assert_eq!(f.size, Some(4242));
        assert!(f.body.is_none());
    }

    #[test]
    fn extracts_body_literal_as_bytes() {
        let input = b"* 1 FETCH (UID 5 BODY[] {11}\r\nHello world)\r\n";
        let f = parse_fetch(input);
        assert_eq!(f.body.as_deref(), Some(&b"Hello world"[..]));
    }

    #[test]
    fn ignores_unknown_attrs() {
        let f = parse_fetch(b"* 1 FETCH (UID 1 X-CUSTOM \"ignored\" MODSEQ (12345))\r\n");
        assert_eq!(f.uid, Some(1));
        assert!(f.flags.is_empty());
        assert!(f.size.is_none());
    }

    #[test]
    fn empty_flags_list() {
        let f = parse_fetch(b"* 1 FETCH (UID 7 FLAGS ())\r\n");
        assert_eq!(f.uid, Some(7));
        assert!(f.flags.is_empty());
    }
}
