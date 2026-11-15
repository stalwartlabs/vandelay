/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::base64::STANDARD;

const BEARER: &str = "auth=Bearer ";
const SEP: char = '\x01';

#[derive(Debug, Clone)]
pub enum SaslAuth {
    Basic { user: String, password: String },
    Bearer { user: String, token: String },
}

impl SaslAuth {
    pub fn user(&self) -> &str {
        match self {
            SaslAuth::Basic { user, .. } | SaslAuth::Bearer { user, .. } => user,
        }
    }
}

#[derive(Clone, Copy)]
pub enum SaslPayload<'a> {
    Plain { authcid: &'a str, password: &'a str },
    XOauth2 { user: &'a str, token: &'a str },
    OAuthBearer { user: &'a str, token: &'a str },
}

impl SaslPayload<'_> {
    pub fn mechanism(self) -> &'static str {
        match self {
            SaslPayload::Plain { .. } => "PLAIN",
            SaslPayload::XOauth2 { .. } => "XOAUTH2",
            SaslPayload::OAuthBearer { .. } => "OAUTHBEARER",
        }
    }

    pub fn bytes(self) -> Vec<u8> {
        match self {
            SaslPayload::Plain { authcid, password } => {
                let mut out = Vec::with_capacity(authcid.len() + password.len() + 2);
                out.push(0);
                out.extend_from_slice(authcid.as_bytes());
                out.push(0);
                out.extend_from_slice(password.as_bytes());
                out
            }
            SaslPayload::XOauth2 { user, token } => {
                let mut out = String::with_capacity(user.len() + token.len() + 32);
                out.push_str("user=");
                out.push_str(user);
                out.push(SEP);
                out.push_str(BEARER);
                out.push_str(token);
                out.push(SEP);
                out.push(SEP);
                out.into_bytes()
            }
            SaslPayload::OAuthBearer { user, token } => {
                let mut out = String::with_capacity(user.len() + token.len() + 32);
                out.push_str("n,a=");
                out.push_str(user);
                out.push(',');
                out.push(SEP);
                out.push_str(BEARER);
                out.push_str(token);
                out.push(SEP);
                out.push(SEP);
                out.into_bytes()
            }
        }
    }

    pub fn base64(self) -> String {
        STANDARD.encode(self.bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_match_the_wire_formats() {
        let plain = SaslPayload::Plain {
            authcid: "foo",
            password: "bar",
        };
        assert_eq!(plain.mechanism(), "PLAIN");
        assert_eq!(plain.bytes(), b"\0foo\0bar");
        assert_eq!(plain.base64(), "AGZvbwBiYXI=");
        assert_eq!(
            SaslPayload::XOauth2 {
                user: "u",
                token: "t"
            }
            .bytes(),
            b"user=u\x01auth=Bearer t\x01\x01"
        );
        assert_eq!(
            SaslPayload::OAuthBearer {
                user: "u",
                token: "t"
            }
            .bytes(),
            b"n,a=u,\x01auth=Bearer t\x01\x01"
        );
    }
}
