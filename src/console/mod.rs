/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod logo;
mod table;
mod units;

pub use logo::Logo;
pub use table::{Align, Column, Table};
pub use units::{ByteSize, Grouped};

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display};
use std::io::{self, IsTerminal};

use clap::ValueEnum;

const NO_COLOR: &str = "NO_COLOR";
const TERM: &str = "TERM";
const DUMB_TERM: &str = "dumb";
const COLOR_FLAG: &str = "--color";
const COLOR_FLAG_EQ: &str = "--color=";
const END_OF_FLAGS: &str = "--";
const RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    pub fn from_args(args: impl IntoIterator<Item = OsString>) -> ColorChoice {
        let mut choice = ColorChoice::Auto;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let Some(arg) = arg.to_str() else {
                continue;
            };
            let value = if arg == END_OF_FLAGS {
                break;
            } else if arg == COLOR_FLAG {
                args.next()
                    .and_then(|v| v.to_str().and_then(ColorChoice::parse))
            } else {
                arg.strip_prefix(COLOR_FLAG_EQ).and_then(ColorChoice::parse)
            };
            if let Some(value) = value {
                choice = value;
            }
        }
        choice
    }

    fn parse(value: &str) -> Option<ColorChoice> {
        ColorChoice::from_str(value, true).ok()
    }

    fn styled(self, is_terminal: bool) -> Styled {
        match self {
            ColorChoice::Always => Styled::Ansi,
            ColorChoice::Never => Styled::Plain,
            ColorChoice::Auto if is_terminal => Styled::Ansi,
            ColorChoice::Auto => Styled::Plain,
        }
    }

    fn effective(self, no_color: Option<&OsStr>, term: Option<&OsStr>) -> ColorChoice {
        let plain_env = no_color.is_some_and(|v| !v.is_empty())
            || term.map_or(!cfg!(windows), |t| t == DUMB_TERM);
        match self {
            ColorChoice::Auto if plain_env => ColorChoice::Never,
            choice => choice,
        }
    }

    pub const fn clap(self) -> clap::ColorChoice {
        match self {
            ColorChoice::Auto => clap::ColorChoice::Auto,
            ColorChoice::Always => clap::ColorChoice::Always,
            ColorChoice::Never => clap::ColorChoice::Never,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Styled {
    #[default]
    Plain,
    Ansi,
}

impl Styled {
    pub fn paint<T: Display>(self, tone: Tone, value: T) -> Paint<T> {
        Paint {
            styled: self,
            tone,
            value,
        }
    }

    pub fn severity(self, severity: Severity) -> Paint<&'static str> {
        self.paint(severity.tone(), severity.prefix())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Console {
    pub out: Styled,
    pub err: Styled,
}

impl Console {
    pub fn detect(choice: ColorChoice) -> Console {
        let choice = choice.effective(
            env::var_os(NO_COLOR).as_deref(),
            env::var_os(TERM).as_deref(),
        );
        Console {
            out: choice.styled(io::stdout().is_terminal()),
            err: choice.styled(io::stderr().is_terminal()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Error,
    Warning,
    Note,
    Heading,
    Title,
    Dim,
    Good,
    Bad,
}

impl Tone {
    const fn sgr(self) -> &'static str {
        match self {
            Tone::Error => "\x1b[1;31m",
            Tone::Warning => "\x1b[1;33m",
            Tone::Note => "\x1b[1;36m",
            Tone::Heading => "\x1b[1m",
            Tone::Title => "\x1b[1;38;2;219;45;84m",
            Tone::Dim => "\x1b[2m",
            Tone::Good => "\x1b[32m",
            Tone::Bad => "\x1b[31m",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    const fn prefix(self) -> &'static str {
        match self {
            Severity::Error => "error:",
            Severity::Warning => "warning:",
            Severity::Note => "note:",
        }
    }

    const fn tone(self) -> Tone {
        match self {
            Severity::Error => Tone::Error,
            Severity::Warning => Tone::Warning,
            Severity::Note => Tone::Note,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Initial<'a> {
    Upper(&'a str),
    Lower(&'a str),
}

impl Display for Initial<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (text, upper) = match *self {
            Initial::Upper(text) => (text, true),
            Initial::Lower(text) => (text, false),
        };
        let mut chars = text.chars();
        let Some(first) = chars.next() else {
            return Ok(());
        };
        let first = if upper {
            first.to_ascii_uppercase()
        } else {
            first.to_ascii_lowercase()
        };
        write!(f, "{first}{}", chars.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counted {
    n: u64,
    one: &'static str,
    many: &'static str,
}

impl Counted {
    pub const fn new(n: u64, one: &'static str, many: &'static str) -> Counted {
        Counted { n, one, many }
    }
}

impl Display for Counted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = if self.n == 1 { self.one } else { self.many };
        write!(f, "{} {unit}", Grouped(self.n))
    }
}

pub struct Paint<T> {
    styled: Styled,
    tone: Tone,
    value: T,
}

impl<T: Display> Display for Paint<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.styled {
            Styled::Plain => self.value.fmt(f),
            Styled::Ansi => {
                f.write_str(self.tone.sgr())?;
                self.value.fmt(f)?;
                f.write_str(RESET)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    // The colour flag is found before clap parses, in both spellings, last one wins.
    #[test]
    fn color_flag_is_read_before_parsing() {
        assert_eq!(
            ColorChoice::from_args(args(&["vandelay", "--help"])),
            ColorChoice::Auto
        );
        assert_eq!(
            ColorChoice::from_args(args(&["vandelay", "--color", "never", "--help"])),
            ColorChoice::Never
        );
        assert_eq!(
            ColorChoice::from_args(args(&["vandelay", "--color=always", "--color=NEVER"])),
            ColorChoice::Never
        );
        assert_eq!(
            ColorChoice::from_args(args(&["vandelay", "--", "--color=never"])),
            ColorChoice::Auto
        );
    }

    // Explicit choices ignore the terminal; auto needs one.
    #[test]
    fn explicit_choice_overrides_terminal_detection() {
        assert_eq!(ColorChoice::Always.styled(false), Styled::Ansi);
        assert_eq!(ColorChoice::Never.styled(true), Styled::Plain);
        assert_eq!(ColorChoice::Auto.styled(false), Styled::Plain);
    }

    // Under auto, NO_COLOR, TERM=dumb and an unset TERM (outside Windows) turn colour off, as clap does.
    #[test]
    fn auto_follows_no_color_and_term() {
        let xterm = Some(OsStr::new("xterm-256color"));
        let dumb = Some(OsStr::new("dumb"));
        let set = Some(OsStr::new("1"));
        let empty = Some(OsStr::new(""));
        assert_eq!(ColorChoice::Auto.effective(None, xterm), ColorChoice::Auto);
        assert_eq!(ColorChoice::Auto.effective(empty, xterm), ColorChoice::Auto);
        assert_eq!(ColorChoice::Auto.effective(set, xterm), ColorChoice::Never);
        assert_eq!(ColorChoice::Auto.effective(None, dumb), ColorChoice::Never);
        assert_eq!(
            ColorChoice::Auto.effective(None, None),
            if cfg!(windows) {
                ColorChoice::Auto
            } else {
                ColorChoice::Never
            }
        );
        assert_eq!(
            ColorChoice::Always.effective(set, dumb),
            ColorChoice::Always
        );
        assert_eq!(
            ColorChoice::Never.effective(None, xterm),
            ColorChoice::Never
        );
    }

    // Only the first letter changes case.
    #[test]
    fn initial_changes_the_first_letter_only() {
        assert_eq!(Initial::Lower("Sieve scripts").to_string(), "sieve scripts");
        assert_eq!(
            Initial::Upper("calendar event").to_string(),
            "Calendar event"
        );
        assert_eq!(Initial::Upper("").to_string(), "");
    }

    // Units follow the count, with thousands grouped.
    #[test]
    fn counted_pluralises() {
        assert_eq!(Counted::new(1, "object", "objects").to_string(), "1 object");
        assert_eq!(
            Counted::new(1234, "object", "objects").to_string(),
            "1,234 objects"
        );
        assert_eq!(Counted::new(0, "item", "items").to_string(), "0 items");
    }

    // Plain painting is the bare value with padding applied; ANSI wraps the padded value.
    #[test]
    fn paint_keeps_padding_inside_the_escape() {
        assert_eq!(format!("{:>4}", Styled::Plain.paint(Tone::Bad, 7)), "   7");
        assert_eq!(
            format!("{:>4}", Styled::Ansi.paint(Tone::Bad, 7)),
            "\x1b[31m   7\x1b[0m"
        );
        assert_eq!(
            Styled::Plain.severity(Severity::Warning).to_string(),
            "warning:"
        );
    }
}
