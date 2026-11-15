/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::str::FromStr;

use calcard::common::timezone::Tz;

const MICROSOFT_TZONE: &str = "tzone://Microsoft/";
const MICROSOFT_UTC: &str = "Utc";
const UTC: &str = "Etc/UTC";

type Iana = &'static str;

macro_rules! windows_zones {
    ($($windows:literal => $iana:literal,)+) => {
        #[cfg(test)]
        const WINDOWS_NAMES: &[&str] = &[$($windows,)+];

        impl ZoneId<'_> {
            pub fn windows_iana(self) -> Option<&'static str> {
                hashify::map_ignore_case!(
                    self.0.trim().as_bytes(),
                    Iana,
                    $($windows => $iana,)+
                )
                .copied()
            }
        }
    };
}

#[derive(Debug, Clone, Copy)]
pub struct ZoneId<'a>(pub &'a str);

impl ZoneId<'_> {
    pub fn iana(self) -> Option<String> {
        let key = self.0.trim();
        if key.is_empty() {
            return None;
        }
        if let Some(rest) = key.strip_prefix(MICROSOFT_TZONE) {
            return rest
                .eq_ignore_ascii_case(MICROSOFT_UTC)
                .then(|| UTC.to_owned());
        }
        if let Some(iana) = self.windows_iana() {
            return Some(iana.to_owned());
        }
        Tz::from_str(key)
            .ok()
            .and_then(|tz| tz.name().map(|name| name.into_owned()))
    }
}

windows_zones! {
    "UTC" => "Etc/UTC",
    "Coordinated Universal Time" => "Etc/UTC",
    "UTC-11" => "Etc/GMT+11",
    "UTC-09" => "Etc/GMT+9",
    "UTC-08" => "Etc/GMT+8",
    "UTC-02" => "Etc/GMT+2",
    "UTC+12" => "Etc/GMT-12",
    "UTC+13" => "Etc/GMT-13",
    "Dateline Standard Time" => "Etc/GMT+12",
    "Aleutian Standard Time" => "America/Adak",
    "Hawaiian Standard Time" => "Pacific/Honolulu",
    "Marquesas Standard Time" => "Pacific/Marquesas",
    "Alaskan Standard Time" => "America/Anchorage",
    "Pacific Standard Time (Mexico)" => "America/Tijuana",
    "Pacific Standard Time" => "America/Los_Angeles",
    "US Mountain Standard Time" => "America/Phoenix",
    "Mountain Standard Time (Mexico)" => "America/Mazatlan",
    "Mountain Standard Time" => "America/Denver",
    "Yukon Standard Time" => "America/Whitehorse",
    "Central America Standard Time" => "America/Guatemala",
    "Central Standard Time" => "America/Chicago",
    "Easter Island Standard Time" => "Pacific/Easter",
    "Central Standard Time (Mexico)" => "America/Mexico_City",
    "Canada Central Standard Time" => "America/Regina",
    "SA Pacific Standard Time" => "America/Bogota",
    "Eastern Standard Time (Mexico)" => "America/Cancun",
    "Eastern Standard Time" => "America/New_York",
    "Haiti Standard Time" => "America/Port-au-Prince",
    "Cuba Standard Time" => "America/Havana",
    "US Eastern Standard Time" => "America/Indianapolis",
    "Turks And Caicos Standard Time" => "America/Grand_Turk",
    "Paraguay Standard Time" => "America/Asuncion",
    "Atlantic Standard Time" => "America/Halifax",
    "Venezuela Standard Time" => "America/Caracas",
    "Central Brazilian Standard Time" => "America/Cuiaba",
    "SA Western Standard Time" => "America/La_Paz",
    "Pacific SA Standard Time" => "America/Santiago",
    "Newfoundland Standard Time" => "America/St_Johns",
    "Tocantins Standard Time" => "America/Araguaina",
    "E. South America Standard Time" => "America/Sao_Paulo",
    "SA Eastern Standard Time" => "America/Cayenne",
    "Argentina Standard Time" => "America/Argentina/Buenos_Aires",
    "Greenland Standard Time" => "America/Godthab",
    "Montevideo Standard Time" => "America/Montevideo",
    "Magallanes Standard Time" => "America/Punta_Arenas",
    "Saint Pierre Standard Time" => "America/Miquelon",
    "Bahia Standard Time" => "America/Bahia",
    "Mid-Atlantic Standard Time" => "Atlantic/South_Georgia",
    "Azores Standard Time" => "Atlantic/Azores",
    "Cape Verde Standard Time" => "Atlantic/Cape_Verde",
    "GMT Standard Time" => "Europe/London",
    "Greenwich Standard Time" => "Atlantic/Reykjavik",
    "Sao Tome Standard Time" => "Africa/Sao_Tome",
    "Morocco Standard Time" => "Africa/Casablanca",
    "W. Europe Standard Time" => "Europe/Berlin",
    "Central Europe Standard Time" => "Europe/Budapest",
    "Romance Standard Time" => "Europe/Paris",
    "Central European Standard Time" => "Europe/Warsaw",
    "W. Central Africa Standard Time" => "Africa/Lagos",
    "Jordan Standard Time" => "Asia/Amman",
    "GTB Standard Time" => "Europe/Bucharest",
    "Middle East Standard Time" => "Asia/Beirut",
    "Egypt Standard Time" => "Africa/Cairo",
    "E. Europe Standard Time" => "Europe/Chisinau",
    "Syria Standard Time" => "Asia/Damascus",
    "West Bank Standard Time" => "Asia/Hebron",
    "South Africa Standard Time" => "Africa/Johannesburg",
    "FLE Standard Time" => "Europe/Kiev",
    "Israel Standard Time" => "Asia/Jerusalem",
    "South Sudan Standard Time" => "Africa/Juba",
    "Kaliningrad Standard Time" => "Europe/Kaliningrad",
    "Sudan Standard Time" => "Africa/Khartoum",
    "Libya Standard Time" => "Africa/Tripoli",
    "Namibia Standard Time" => "Africa/Windhoek",
    "Arabic Standard Time" => "Asia/Baghdad",
    "Turkey Standard Time" => "Europe/Istanbul",
    "Arab Standard Time" => "Asia/Riyadh",
    "Belarus Standard Time" => "Europe/Minsk",
    "Russian Standard Time" => "Europe/Moscow",
    "E. Africa Standard Time" => "Africa/Nairobi",
    "Iran Standard Time" => "Asia/Tehran",
    "Arabian Standard Time" => "Asia/Dubai",
    "Astrakhan Standard Time" => "Europe/Astrakhan",
    "Azerbaijan Standard Time" => "Asia/Baku",
    "Russia Time Zone 3" => "Europe/Samara",
    "Mauritius Standard Time" => "Indian/Mauritius",
    "Saratov Standard Time" => "Europe/Saratov",
    "Georgian Standard Time" => "Asia/Tbilisi",
    "Volgograd Standard Time" => "Europe/Volgograd",
    "Caucasus Standard Time" => "Asia/Yerevan",
    "Afghanistan Standard Time" => "Asia/Kabul",
    "West Asia Standard Time" => "Asia/Tashkent",
    "Ekaterinburg Standard Time" => "Asia/Yekaterinburg",
    "Pakistan Standard Time" => "Asia/Karachi",
    "Qyzylorda Standard Time" => "Asia/Qyzylorda",
    "India Standard Time" => "Asia/Kolkata",
    "Sri Lanka Standard Time" => "Asia/Colombo",
    "Nepal Standard Time" => "Asia/Katmandu",
    "Central Asia Standard Time" => "Asia/Almaty",
    "Bangladesh Standard Time" => "Asia/Dhaka",
    "Omsk Standard Time" => "Asia/Omsk",
    "Myanmar Standard Time" => "Asia/Rangoon",
    "SE Asia Standard Time" => "Asia/Bangkok",
    "Altai Standard Time" => "Asia/Barnaul",
    "W. Mongolia Standard Time" => "Asia/Hovd",
    "North Asia Standard Time" => "Asia/Krasnoyarsk",
    "N. Central Asia Standard Time" => "Asia/Novosibirsk",
    "Tomsk Standard Time" => "Asia/Tomsk",
    "China Standard Time" => "Asia/Shanghai",
    "North Asia East Standard Time" => "Asia/Irkutsk",
    "Singapore Standard Time" => "Asia/Singapore",
    "W. Australia Standard Time" => "Australia/Perth",
    "Taipei Standard Time" => "Asia/Taipei",
    "Ulaanbaatar Standard Time" => "Asia/Ulaanbaatar",
    "Aus Central W. Standard Time" => "Australia/Eucla",
    "Transbaikal Standard Time" => "Asia/Chita",
    "Tokyo Standard Time" => "Asia/Tokyo",
    "North Korea Standard Time" => "Asia/Pyongyang",
    "Korea Standard Time" => "Asia/Seoul",
    "Yakutsk Standard Time" => "Asia/Yakutsk",
    "Cen. Australia Standard Time" => "Australia/Adelaide",
    "AUS Central Standard Time" => "Australia/Darwin",
    "E. Australia Standard Time" => "Australia/Brisbane",
    "AUS Eastern Standard Time" => "Australia/Sydney",
    "West Pacific Standard Time" => "Pacific/Port_Moresby",
    "Tasmania Standard Time" => "Australia/Hobart",
    "Vladivostok Standard Time" => "Asia/Vladivostok",
    "Lord Howe Standard Time" => "Australia/Lord_Howe",
    "Bougainville Standard Time" => "Pacific/Bougainville",
    "Russia Time Zone 10" => "Asia/Srednekolymsk",
    "Magadan Standard Time" => "Asia/Magadan",
    "Norfolk Standard Time" => "Pacific/Norfolk",
    "Sakhalin Standard Time" => "Asia/Sakhalin",
    "Central Pacific Standard Time" => "Pacific/Guadalcanal",
    "Russia Time Zone 11" => "Asia/Kamchatka",
    "New Zealand Standard Time" => "Pacific/Auckland",
    "Fiji Standard Time" => "Pacific/Fiji",
    "Kamchatka Standard Time" => "Asia/Kamchatka",
    "Chatham Islands Standard Time" => "Pacific/Chatham",
    "Tonga Standard Time" => "Pacific/Tongatapu",
    "Samoa Standard Time" => "Pacific/Apia",
    "Line Islands Standard Time" => "Pacific/Kiritimati",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacific_standard_time_maps_to_los_angeles() {
        assert_eq!(
            ZoneId("Pacific Standard Time").windows_iana(),
            Some("America/Los_Angeles")
        );
    }

    #[test]
    fn romance_standard_time_maps_to_paris() {
        assert_eq!(
            ZoneId("Romance Standard Time").windows_iana(),
            Some("Europe/Paris")
        );
    }

    #[test]
    fn unknown_zone_returns_none() {
        assert_eq!(ZoneId("Made Up Zone").windows_iana(), None);
    }

    #[test]
    fn case_insensitive_lookup() {
        assert_eq!(
            ZoneId("eastern standard time").windows_iana(),
            Some("America/New_York")
        );
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            ZoneId("  GMT Standard Time  ").windows_iana(),
            Some("Europe/London")
        );
    }

    #[test]
    fn full_cldr_table_has_at_least_140_rows_and_each_resolves() {
        // Every Windows name in the table resolves, in any case.
        assert!(
            WINDOWS_NAMES.len() >= 140,
            "got {} rows",
            WINDOWS_NAMES.len()
        );
        for name in WINDOWS_NAMES {
            assert!(ZoneId(name).windows_iana().is_some(), "{name}");
            assert!(
                ZoneId(&name.to_ascii_uppercase()).windows_iana().is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn spot_check_coverage_across_continents() {
        assert_eq!(
            ZoneId("Tokyo Standard Time").windows_iana(),
            Some("Asia/Tokyo")
        );
        assert_eq!(
            ZoneId("AUS Eastern Standard Time").windows_iana(),
            Some("Australia/Sydney")
        );
        assert_eq!(
            ZoneId("Cen. Australia Standard Time").windows_iana(),
            Some("Australia/Adelaide")
        );
        assert_eq!(
            ZoneId("Greenland Standard Time").windows_iana(),
            Some("America/Godthab")
        );
        assert_eq!(
            ZoneId("China Standard Time").windows_iana(),
            Some("Asia/Shanghai")
        );
        assert_eq!(
            ZoneId("Israel Standard Time").windows_iana(),
            Some("Asia/Jerusalem")
        );
        assert_eq!(
            ZoneId("Chatham Islands Standard Time").windows_iana(),
            Some("Pacific/Chatham")
        );
        assert_eq!(
            ZoneId("Newfoundland Standard Time").windows_iana(),
            Some("America/St_Johns")
        );
        assert_eq!(ZoneId("UTC-08").windows_iana(), Some("Etc/GMT+8"));
        assert_eq!(ZoneId("UTC+13").windows_iana(), Some("Etc/GMT-13"));
        assert_eq!(
            ZoneId("Yukon Standard Time").windows_iana(),
            Some("America/Whitehorse")
        );
        assert_eq!(
            ZoneId("Aus Central W. Standard Time").windows_iana(),
            Some("Australia/Eucla")
        );
    }

    #[test]
    fn resolve_handles_windows_iana_offset_and_microsoft_tzone() {
        assert_eq!(
            ZoneId("Romance Standard Time").iana().as_deref(),
            Some("Europe/Paris")
        );
        assert_eq!(
            ZoneId("Europe/Paris").iana().as_deref(),
            Some("Europe/Paris")
        );
        assert_eq!(ZoneId("UTC").iana().as_deref(), Some("Etc/UTC"));
        assert_eq!(
            ZoneId("tzone://Microsoft/Utc").iana().as_deref(),
            Some("Etc/UTC")
        );
        assert_eq!(ZoneId("tzone://Microsoft/Custom").iana(), None);
        assert_eq!(ZoneId("Made Up Zone").iana(), None);
        assert_eq!(ZoneId("").iana(), None);
    }

    #[test]
    fn coordinated_universal_time_alias_resolves_to_utc() {
        assert_eq!(
            ZoneId("Coordinated Universal Time").windows_iana(),
            Some("Etc/UTC")
        );
    }
}
