/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::{fmt, str};

const MAX_GROUPED_LEN: usize = 26;
const BYTE_UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
const KIB: u64 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grouped(pub u64);

impl Grouped {
    pub fn width(self) -> usize {
        let digits = self.0.checked_ilog10().map_or(1, |l| l as usize + 1);
        digits + (digits - 1) / 3
    }

    fn render(self, buf: &mut [u8; MAX_GROUPED_LEN]) -> &str {
        let mut n = self.0;
        let mut start = MAX_GROUPED_LEN;
        let mut digits = 0;
        for slot in buf.iter_mut().rev() {
            start -= 1;
            if digits == 3 {
                *slot = b',';
                digits = 0;
                continue;
            }
            *slot = b'0' + (n % 10) as u8;
            digits += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        buf.get(start..)
            .and_then(|s| str::from_utf8(s).ok())
            .unwrap_or_default()
    }
}

impl fmt::Display for Grouped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; MAX_GROUPED_LEN];
        f.pad(self.render(&mut buf))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < KIB {
            return write!(f, "{} B", self.0);
        }
        let [mut unit, larger @ ..] = BYTE_UNITS;
        let mut value = self.0 as f64;
        for next in larger {
            if value < KIB as f64 {
                break;
            }
            value /= KIB as f64;
            unit = next;
        }
        write!(f, "{value:.1} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_count(n: u64) -> String {
        let s = n.to_string();
        let bytes = s.as_bytes();
        let len = bytes.len();
        let mut out = String::with_capacity(len + len / 3);
        for (i, ch) in bytes.iter().enumerate() {
            if i > 0 && (len - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(*ch as char);
        }
        out
    }

    fn reference_bytes(n: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        if n < 1024 {
            return format!("{n} B");
        }
        let mut value = n as f64;
        let mut i = 0;
        while value >= 1024.0 && i + 1 < UNITS.len() {
            value /= 1024.0;
            i += 1;
        }
        format!("{value:.1} {}", UNITS[i])
    }

    fn samples() -> impl Iterator<Item = u64> {
        (0..64u32)
            .flat_map(|shift| {
                let p = 1u64 << shift;
                [p - 1, p, p + 1, p.saturating_mul(10), p / 3]
            })
            .chain([0, 9, 10, 99, 100, 999, 1_000, 1_234_567_890, u64::MAX])
            .chain((0..20).map(|e| 10u64.pow(e)))
    }

    #[test]
    fn grouped_inserts_commas() {
        assert_eq!(Grouped(0).to_string(), "0");
        assert_eq!(Grouped(999).to_string(), "999");
        assert_eq!(Grouped(1_000).to_string(), "1,000");
        assert_eq!(Grouped(1_234_567_890).to_string(), "1,234,567,890");
        assert_eq!(Grouped(u64::MAX).to_string(), "18,446,744,073,709,551,615");
    }

    #[test]
    fn grouped_matches_the_reference_and_pads() {
        for n in samples() {
            let expected = reference_count(n);
            assert_eq!(Grouped(n).to_string(), expected, "{n}");
            assert_eq!(Grouped(n).width(), expected.len(), "{n}");
            assert_eq!(format!("{:>30}", Grouped(n)), format!("{expected:>30}"));
        }
    }

    #[test]
    fn byte_size_matches_the_reference() {
        assert_eq!(ByteSize(512).to_string(), "512 B");
        assert_eq!(ByteSize(1_536).to_string(), "1.5 KB");
        for n in samples() {
            assert_eq!(ByteSize(n).to_string(), reference_bytes(n), "{n}");
        }
    }
}
