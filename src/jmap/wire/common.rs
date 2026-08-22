/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JmapId(pub String);

impl From<String> for JmapId {
    fn from(value: String) -> Self {
        JmapId(value)
    }
}

impl From<&str> for JmapId {
    fn from(value: &str) -> Self {
        JmapId(value.to_owned())
    }
}

impl std::fmt::Display for JmapId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type UtcDate = time::OffsetDateTime;

pub type Date = time::OffsetDateTime;

pub fn bool_or_true<'de, D>(d: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(true))
}

pub fn lenient_utc_date<'de, D>(d: D) -> Result<Option<UtcDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(d)?
        .as_deref()
        .and_then(parse_utc_date))
}

pub fn parse_utc_date(raw: &str) -> Option<UtcDate> {
    use time::format_description::well_known::Rfc3339;
    let trimmed = raw.trim();
    if let Ok(dt) = UtcDate::parse(trimmed, &Rfc3339) {
        return Some(dt);
    }
    let separated = trimmed.replacen(' ', "T", 1);
    if let Ok(dt) = UtcDate::parse(&separated, &Rfc3339) {
        return Some(dt);
    }
    UtcDate::parse(&format!("{separated}Z"), &Rfc3339).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_rfc3339_is_accepted() {
        let dt = parse_utc_date("2026-08-22T09:42:52Z").expect("parsed");
        assert_eq!(dt.year(), 2026);
        assert!(parse_utc_date("2026-08-22T09:42:52.123+02:00").is_some());
    }

    #[test]
    fn space_separator_and_missing_zone_are_salvaged() {
        assert!(parse_utc_date("2026-08-22 09:42:52Z").is_some());
        assert!(parse_utc_date("2026-08-22T09:42:52").is_some());
        assert!(parse_utc_date("  2026-08-22T09:42:52Z  ").is_some());
    }

    #[test]
    fn unrepresentable_dates_yield_none_instead_of_an_error() {
        assert!(parse_utc_date("30828-09-14T02:48:05Z").is_none());
        assert!(parse_utc_date("10000-01-01T00:00:00Z").is_none());
        assert!(parse_utc_date("").is_none());
        assert!(parse_utc_date("not a date").is_none());
    }
}
