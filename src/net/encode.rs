/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

const HEX: &[u8; 16] = b"0123456789ABCDEF";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeSet {
    Unreserved,
    Path,
    MediaTypeUri,
}

impl SafeSet {
    const UNRESERVED: u128 = SafeSet::mask(b"-_.~");
    const PATH: u128 = SafeSet::mask(b"-_.~/@");
    const MEDIA_TYPE_URI: u128 = SafeSet::mask(b"!$&'()*+-./:=@_~");

    const fn mask(extra: &[u8]) -> u128 {
        let mut mask = 0u128;
        let mut b = b'0';
        while b <= b'9' {
            mask |= 1 << b;
            b += 1;
        }
        b = b'A';
        while b <= b'Z' {
            mask |= 1 << b;
            mask |= 1 << (b + 32);
            b += 1;
        }
        let mut rest = extra;
        while let [first, tail @ ..] = rest {
            mask |= 1 << *first;
            rest = tail;
        }
        mask
    }

    fn bits(self) -> u128 {
        match self {
            SafeSet::Unreserved => SafeSet::UNRESERVED,
            SafeSet::Path => SafeSet::PATH,
            SafeSet::MediaTypeUri => SafeSet::MEDIA_TYPE_URI,
        }
    }

    pub fn contains(self, byte: u8) -> bool {
        byte < 128 && self.bits() & (1 << byte) != 0
    }
}

pub struct Percent;

impl Percent {
    pub fn encode_into(out: &mut String, input: &str, safe: SafeSet) {
        out.reserve(input.len());
        for byte in input.bytes() {
            if safe.contains(byte) {
                out.push(char::from(byte));
            } else {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0F)]));
            }
        }
    }

    pub fn encode(input: &str, safe: SafeSet) -> String {
        let mut out = String::with_capacity(input.len());
        Percent::encode_into(&mut out, input, safe);
        out
    }
}

#[derive(Debug, Default, Clone)]
pub struct FormBody(String);

impl FormBody {
    pub fn new() -> FormBody {
        FormBody::default()
    }

    pub fn from_pairs(pairs: &[(&str, &str)]) -> FormBody {
        let mut body = FormBody::new();
        for (key, value) in pairs {
            body.pair(key, value);
        }
        body
    }

    pub fn pair(&mut self, key: &str, value: &str) -> &mut FormBody {
        if !self.0.is_empty() {
            self.0.push('&');
        }
        Percent::encode_into(&mut self.0, key, SafeSet::Unreserved);
        self.0.push('=');
        Percent::encode_into(&mut self.0, value, SafeSet::Unreserved);
        self
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(s: &str, safe: impl Fn(u8) -> bool) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            if safe(b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    #[test]
    fn sets_match_the_replaced_encoders_on_every_byte() {
        let all: String = (1u8..128)
            .map(char::from)
            .chain("é€\u{10348}".chars())
            .collect();
        let unreserved =
            |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        let path = |b: u8| unreserved(b) || matches!(b, b'/' | b'@');
        let media = |b: u8| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'/'
                        | b':'
                        | b'='
                        | b'@'
                        | b'_'
                        | b'~'
                )
        };
        assert_eq!(
            Percent::encode(&all, SafeSet::Unreserved),
            reference(&all, unreserved)
        );
        assert_eq!(Percent::encode(&all, SafeSet::Path), reference(&all, path));
        assert_eq!(
            Percent::encode(&all, SafeSet::MediaTypeUri),
            reference(&all, media)
        );
    }

    #[test]
    fn form_body_joins_encoded_pairs() {
        let body = FormBody::from_pairs(&[("grant_type", "refresh_token"), ("scope", "a b+c/d")]);
        assert_eq!(
            body.as_str(),
            "grant_type=refresh_token&scope=a%20b%2Bc%2Fd"
        );
        assert_eq!(FormBody::new().into_string(), "");
    }
}
