/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt;

use super::name::SieveString;

#[derive(Debug, Clone, Copy)]
pub enum Command<'a> {
    Capability,
    StartTls,
    Logout,
    ListScripts,
    GetScript(&'a str),
    Authenticate {
        mechanism: &'a str,
        initial: Option<&'a str>,
    },
    Continuation(&'a str),
}

impl fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Command::Capability => f.write_str("CAPABILITY")?,
            Command::StartTls => f.write_str("STARTTLS")?,
            Command::Logout => f.write_str("LOGOUT")?,
            Command::ListScripts => f.write_str("LISTSCRIPTS")?,
            Command::GetScript(name) => write!(f, "GETSCRIPT {}", SieveString(name))?,
            Command::Authenticate {
                mechanism,
                initial: None,
            } => write!(f, "AUTHENTICATE {}", SieveString(mechanism))?,
            Command::Authenticate {
                mechanism,
                initial: Some(initial),
            } => write!(
                f,
                "AUTHENTICATE {} {}",
                SieveString(mechanism),
                SieveString(initial)
            )?,
            Command::Continuation(payload) => write!(f, "{}", SieveString(payload))?,
        }
        f.write_str("\r\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_is_uppercase_with_crlf() {
        assert_eq!(Command::Capability.to_string(), "CAPABILITY\r\n");
    }

    #[test]
    fn getscript_quoted_for_ascii_names() {
        assert_eq!(
            Command::GetScript("vacation").to_string(),
            "GETSCRIPT \"vacation\"\r\n"
        );
    }

    #[test]
    fn getscript_escapes_quotes_and_backslashes() {
        let out = Command::GetScript("a\"b\\c").to_string();
        assert!(out.contains("\\\""));
        assert!(out.contains("\\\\"));
    }

    #[test]
    fn getscript_uses_literal_for_non_ascii() {
        let name = "fancy\u{00e7}y";
        let out = Command::GetScript(name).to_string();
        let nbytes = name.len();
        let expected = {
            let mut v: Vec<u8> = Vec::new();
            v.extend_from_slice(format!("GETSCRIPT {{{nbytes}+}}\r\n").as_bytes());
            v.extend_from_slice(name.as_bytes());
            v.extend_from_slice(b"\r\n");
            v
        };
        assert_eq!(
            out.as_bytes(),
            expected.as_slice(),
            "GETSCRIPT wire bytes for non-ascii name must be byte-exact"
        );
    }

    #[test]
    fn getscript_uses_literal_for_names_containing_crlf() {
        let name = "with\nlinebreak";
        let out = Command::GetScript(name).to_string();
        assert!(
            out.contains("{") && out.contains("+}\r\n"),
            "expected literal for newline-bearing name: {out:?}"
        );
    }

    #[test]
    fn authenticate_with_initial_emits_two_arguments() {
        let out = Command::Authenticate {
            mechanism: "PLAIN",
            initial: Some("AGZvbwBiYXI="),
        }
        .to_string();
        assert_eq!(out, "AUTHENTICATE \"PLAIN\" \"AGZvbwBiYXI=\"\r\n");
    }

    #[test]
    fn authenticate_without_initial_emits_one_argument() {
        let out = Command::Authenticate {
            mechanism: "LOGIN",
            initial: None,
        }
        .to_string();
        assert_eq!(out, "AUTHENTICATE \"LOGIN\"\r\n");
    }

    #[test]
    fn continuation_payload_quotes_simple_base64() {
        assert_eq!(Command::Continuation("Zm9v").to_string(), "\"Zm9v\"\r\n");
    }
}
