/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

use quick_xml::events::BytesRef;

const SPECIAL: [char; 5] = ['&', '<', '>', '"', '\''];

#[derive(Debug, Clone, Copy)]
pub struct XmlEscape<'a>(pub &'a str);

impl XmlEscape<'_> {
    fn entity(special: u8) -> &'static str {
        match special {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            _ => "&apos;",
        }
    }
}

impl fmt::Display for XmlEscape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for piece in self.0.split_inclusive(SPECIAL) {
            match (piece.strip_suffix(SPECIAL), piece.as_bytes().last()) {
                (Some(text), Some(&special)) => {
                    f.write_str(text)?;
                    f.write_str(XmlEscape::entity(special))?;
                }
                _ => f.write_str(piece)?,
            }
        }
        Ok(())
    }
}

pub struct Entity;

impl Entity {
    pub fn resolve(name: &[u8]) -> Option<char> {
        hashify::map!(
            name,
            char,
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
        )
        .copied()
    }

    pub fn from_ref(reference: &BytesRef<'_>) -> Option<char> {
        match reference.resolve_char_ref() {
            Ok(Some(c)) => Some(c),
            _ => Entity::resolve(reference.as_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_all_five_entities() {
        assert_eq!(
            XmlEscape("a&b<c>d\"e'f").to_string(),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(XmlEscape("").to_string(), "");
        assert_eq!(XmlEscape("plain é").to_string(), "plain é");
        assert_eq!(XmlEscape("<<").to_string(), "&lt;&lt;");
    }

    #[test]
    fn resolves_named_and_numeric_references() {
        assert_eq!(Entity::resolve(b"amp"), Some('&'));
        assert_eq!(Entity::resolve(b"apos"), Some('\''));
        assert_eq!(Entity::resolve(b"nbsp"), None);
        assert_eq!(Entity::from_ref(&BytesRef::new("quot")), Some('"'));
        assert_eq!(Entity::from_ref(&BytesRef::new("#x41")), Some('A'));
        assert_eq!(Entity::from_ref(&BytesRef::new("#66")), Some('B'));
        assert_eq!(Entity::from_ref(&BytesRef::new("bogus")), None);
    }
}
