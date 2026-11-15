/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

const MAX_KEYWORD_LEN: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemKeyword {
    Seen,
    Draft,
    Flagged,
    Answered,
    Forwarded,
    Deleted,
}

impl SystemKeyword {
    pub fn as_str(self) -> &'static str {
        match self {
            SystemKeyword::Seen => "$seen",
            SystemKeyword::Draft => "$draft",
            SystemKeyword::Flagged => "$flagged",
            SystemKeyword::Answered => "$answered",
            SystemKeyword::Forwarded => "$forwarded",
            SystemKeyword::Deleted => "$deleted",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keywords(Vec<Box<str>>);

impl Keywords {
    pub fn new() -> Keywords {
        Keywords::default()
    }

    pub fn with_capacity(capacity: usize) -> Keywords {
        Keywords(Vec::with_capacity(capacity))
    }

    pub fn contains(&self, keyword: &str) -> bool {
        self.0.iter().any(|k| k.eq_ignore_ascii_case(keyword))
    }

    pub fn push(&mut self, keyword: &str) {
        if !self.contains(keyword) {
            self.0.push(keyword.into());
        }
    }

    pub fn insert(&mut self, keyword: String) -> bool {
        let fresh = !self.contains(&keyword);
        if fresh {
            self.0.push(keyword.into_boxed_str());
        }
        fresh
    }

    pub fn push_system(&mut self, keyword: SystemKeyword) {
        self.push(keyword.as_str());
    }

    pub fn from_phrase(phrase: &str) -> Option<String> {
        let mut keyword = String::with_capacity(phrase.len().min(MAX_KEYWORD_LEN));
        let mut separator = false;
        for ch in phrase.chars() {
            if ch.is_ascii_graphic()
                && !matches!(ch, '(' | ')' | '{' | ']' | '%' | '*' | '"' | '\\')
                && !(ch == '$' && keyword.is_empty())
            {
                let sep = separator && !keyword.is_empty();
                if keyword.len() + usize::from(sep) >= MAX_KEYWORD_LEN {
                    break;
                }
                if sep {
                    keyword.push('_');
                }
                keyword.push(ch.to_ascii_lowercase());
                separator = false;
            } else {
                separator = true;
            }
        }
        (!keyword.is_empty()).then_some(keyword)
    }

    pub fn push_category(&mut self, phrase: &str) {
        if let Some(keyword) = Keywords::from_phrase(phrase)
            && !self.contains(&keyword)
        {
            self.0.push(keyword.into_boxed_str());
        }
    }

    pub fn sort(&mut self) {
        self.0.sort_unstable();
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(AsRef::as_ref)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_else(|_| "[]".to_owned())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageFlags {
    pub read: bool,
    pub draft: bool,
    pub flagged: bool,
}

impl MessageFlags {
    pub fn keywords(self) -> Keywords {
        let mut keywords = Keywords::with_capacity(3);
        for (set, keyword) in [
            (self.read, SystemKeyword::Seen),
            (self.draft, SystemKeyword::Draft),
            (self.flagged, SystemKeyword::Flagged),
        ] {
            if set {
                keywords.push_system(keyword);
            }
        }
        keywords
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn push_keeps_order_and_dedups_case_insensitively() {
        let mut k = Keywords::new();
        k.push("$seen");
        k.push("Work");
        k.push("$SEEN");
        k.push("work");
        k.push_system(SystemKeyword::Deleted);
        assert_eq!(k.iter().collect::<Vec<_>>(), ["$seen", "Work", "$deleted"]);
    }

    #[test]
    fn sort_orders_bytewise() {
        let mut k = Keywords::new();
        for kw in ["$seen", "$answered", "$flagged"] {
            k.push(kw);
        }
        k.sort();
        assert_eq!(k.to_json(), r#"["$answered","$flagged","$seen"]"#);
    }

    #[test]
    fn json_matches_the_value_array_encoding() {
        let mut k = Keywords::new();
        k.push("a\"b");
        k.push("ü");
        let reference =
            Value::Array(k.iter().map(|s| Value::String(s.to_owned())).collect()).to_string();
        assert_eq!(k.to_json(), reference);
        assert_eq!(Keywords::new().to_json(), "[]");
    }

    #[test]
    fn message_flags_then_categories() {
        let mut k = MessageFlags {
            read: true,
            draft: false,
            flagged: true,
        }
        .keywords();
        k.push_category("Review / Legal");
        k.push_category("review / legal");
        k.push_category("   ");
        assert_eq!(
            k.iter().collect::<Vec<_>>(),
            ["$seen", "$flagged", "review_/_legal"]
        );
    }
}
