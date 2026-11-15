/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339 as Rfc3339Format;

pub struct Rfc3339;

impl Rfc3339 {
    pub const EPOCH: &str = "1970-01-01T00:00:00Z";

    pub fn now() -> String {
        Rfc3339::format(OffsetDateTime::now_utc())
    }

    pub fn from_unix(secs: u64) -> String {
        i64::try_from(secs)
            .ok()
            .and_then(|secs| OffsetDateTime::from_unix_timestamp(secs).ok())
            .map_or_else(|| Rfc3339::EPOCH.to_owned(), Rfc3339::format)
    }

    fn format(when: OffsetDateTime) -> String {
        when.format(&Rfc3339Format)
            .unwrap_or_else(|_| Rfc3339::EPOCH.to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingDate {
    Now,
    Epoch,
}

impl MissingDate {
    pub fn resolve(self) -> String {
        match self {
            MissingDate::Now => Rfc3339::now(),
            MissingDate::Epoch => Rfc3339::EPOCH.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn reference(secs: u64) -> String {
        OffsetDateTime::from(UNIX_EPOCH + Duration::from_secs(secs))
            .format(&Rfc3339Format)
            .unwrap()
    }

    #[test]
    fn from_unix_matches_the_replaced_helper() {
        for secs in [0, 1, 784_111_777, 1_700_000_000, 253_402_300_799] {
            assert_eq!(Rfc3339::from_unix(secs), reference(secs), "{secs}");
        }
        assert_eq!(Rfc3339::from_unix(0), Rfc3339::EPOCH);
    }

    #[test]
    fn out_of_range_falls_back_to_epoch() {
        assert_eq!(Rfc3339::from_unix(u64::MAX), Rfc3339::EPOCH);
    }

    #[test]
    fn missing_date_policies() {
        assert_eq!(MissingDate::Epoch.resolve(), Rfc3339::EPOCH);
        let now = MissingDate::Now.resolve();
        assert!(OffsetDateTime::parse(&now, &Rfc3339Format).is_ok());
        assert_ne!(now, Rfc3339::EPOCH);
    }
}
