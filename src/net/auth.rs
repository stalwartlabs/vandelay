/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use encodify::base64::STANDARD;

const BASIC: &str = "Basic ";

#[derive(Debug, Clone)]
pub enum Auth {
    Basic { user: String, password: String },
    Bearer { token: String },
}

impl Auth {
    pub fn header_value(&self) -> String {
        match self {
            Auth::Basic { user, password } => {
                let credentials = format!("{user}:{password}");
                let mut header =
                    String::with_capacity(BASIC.len() + STANDARD.encoded_len(credentials.len()));
                header.push_str(BASIC);
                STANDARD.encode_append(credentials, &mut header);
                header
            }
            Auth::Bearer { token } => format!("Bearer {token}"),
        }
    }
}
