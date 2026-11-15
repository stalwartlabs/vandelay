/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::io;

use terminal_size::{Width, terminal_size_of};

use super::Styled;

const LARGE_LOGO_COLUMNS: u16 = 91;
const LARGE_ANS: &str = include_str!("logo/large.ans").trim_ascii_end();
const LARGE_TXT: &str = include_str!("logo/large.txt").trim_ascii_end();
const COMPACT_ANS: &str = include_str!("logo/compact.ans").trim_ascii_end();
const COMPACT_TXT: &str = include_str!("logo/compact.txt").trim_ascii_end();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logo {
    Large,
    Compact,
}

impl Logo {
    pub fn for_stdout() -> Logo {
        Logo::fitting(terminal_size_of(io::stdout()).map(|(Width(columns), _)| columns))
    }

    pub fn for_stderr() -> Logo {
        Logo::fitting(terminal_size_of(io::stderr()).map(|(Width(columns), _)| columns))
    }

    fn fitting(columns: Option<u16>) -> Logo {
        match columns {
            Some(columns) if columns >= LARGE_LOGO_COLUMNS => Logo::Large,
            _ => Logo::Compact,
        }
    }

    pub const fn text(self, styled: Styled) -> &'static str {
        match (self, styled) {
            (Logo::Large, Styled::Ansi) => LARGE_ANS,
            (Logo::Large, Styled::Plain) => LARGE_TXT,
            (Logo::Compact, Styled::Ansi) => COMPACT_ANS,
            (Logo::Compact, Styled::Plain) => COMPACT_TXT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The large logo needs 91 columns; unknown widths get the compact one.
    #[test]
    fn logo_fits_the_terminal_width() {
        assert_eq!(Logo::fitting(Some(LARGE_LOGO_COLUMNS)), Logo::Large);
        assert_eq!(Logo::fitting(Some(LARGE_LOGO_COLUMNS - 1)), Logo::Compact);
        assert_eq!(Logo::fitting(None), Logo::Compact);
    }

    // Plain selects the escape-free variant, and no variant ends in a newline.
    #[test]
    fn logo_variant_follows_style() {
        for logo in [Logo::Large, Logo::Compact] {
            let plain = logo.text(Styled::Plain);
            let colored = logo.text(Styled::Ansi);
            assert!(!plain.contains('\x1b'));
            assert!(colored.contains('\x1b'));
            assert!(!plain.ends_with('\n') && !colored.ends_with('\n'));
        }
        assert!(
            Logo::Large
                .text(Styled::Plain)
                .lines()
                .all(|line| line.chars().count() <= usize::from(LARGE_LOGO_COLUMNS))
        );
    }
}
