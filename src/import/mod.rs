/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

pub mod dav;
pub mod ews;
pub mod graph;
pub mod imap;
pub mod jmap;
pub mod maildir;
pub mod managesieve;
pub mod takeout;

use crate::sync::{RunOptions, RunOutcome};

macro_rules! imports {
    ($($variant:ident($ty:ty),)+) => {
        pub enum Import {
            $($variant($ty),)+
        }

        impl Import {
            pub fn run(self, options: RunOptions) -> RunOutcome {
                match self {
                    $(Import::$variant(import) => import.run(options),)+
                }
            }
        }

        $(
            impl From<$ty> for Import {
                fn from(import: $ty) -> Import {
                    Import::$variant(import)
                }
            }
        )+
    };
}

imports! {
    Jmap(jmap::JmapImport),
    Imap(imap::ImapImport),
    Dav(dav::DavImport),
    ManageSieve(managesieve::ManageSieveImport),
    Maildir(maildir::MaildirImport),
    Takeout(takeout::TakeoutImport),
    Ews(ews::EwsImport),
    Graph(graph::GraphImport),
}
