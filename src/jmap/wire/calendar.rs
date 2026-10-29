/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::JmapId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Calendar {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JmapId>,

    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,

    #[serde(default)]
    pub sort_order: u32,

    #[serde(default = "default_true")]
    pub is_subscribed: bool,

    #[serde(default = "default_true")]
    pub is_visible: bool,

    #[serde(default)]
    pub is_default: bool,

    #[serde(default = "default_include_in_availability")]
    pub include_in_availability: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_alerts_with_time: Option<IndexMap<String, Value>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_alerts_without_time: Option<IndexMap<String, Value>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

impl Calendar {
    pub fn normalize_color(&mut self) -> Option<String> {
        let color = self.color.as_mut()?;
        let trimmed = color.trim();
        let kept = match trimmed.strip_prefix('#') {
            Some(hex) if hex.bytes().all(|byte| byte.is_ascii_hexdigit()) => match hex.len() {
                3 | 4 => trimmed.get(..=3),
                6 | 8 => trimmed.get(..=6),
                _ => None,
            },
            Some(_) => None,
            None => is_css3_color_name(trimmed).then_some(trimmed),
        };
        match kept {
            Some(kept) if kept.len() == color.len() => None,
            Some(kept) => {
                *color = kept.to_owned();
                None
            }
            None => self.color.take(),
        }
    }
}

fn is_css3_color_name(name: &str) -> bool {
    hashify::set_ignore_case!(
        name.as_bytes(),
        "aliceblue",
        "antiquewhite",
        "aqua",
        "aquamarine",
        "azure",
        "beige",
        "bisque",
        "black",
        "blanchedalmond",
        "blue",
        "blueviolet",
        "brown",
        "burlywood",
        "cadetblue",
        "chartreuse",
        "chocolate",
        "coral",
        "cornflowerblue",
        "cornsilk",
        "crimson",
        "cyan",
        "darkblue",
        "darkcyan",
        "darkgoldenrod",
        "darkgray",
        "darkgreen",
        "darkgrey",
        "darkkhaki",
        "darkmagenta",
        "darkolivegreen",
        "darkorange",
        "darkorchid",
        "darkred",
        "darksalmon",
        "darkseagreen",
        "darkslateblue",
        "darkslategray",
        "darkslategrey",
        "darkturquoise",
        "darkviolet",
        "deeppink",
        "deepskyblue",
        "dimgray",
        "dimgrey",
        "dodgerblue",
        "firebrick",
        "floralwhite",
        "forestgreen",
        "fuchsia",
        "gainsboro",
        "ghostwhite",
        "gold",
        "goldenrod",
        "gray",
        "green",
        "greenyellow",
        "grey",
        "honeydew",
        "hotpink",
        "indianred",
        "indigo",
        "ivory",
        "khaki",
        "lavender",
        "lavenderblush",
        "lawngreen",
        "lemonchiffon",
        "lightblue",
        "lightcoral",
        "lightcyan",
        "lightgoldenrodyellow",
        "lightgray",
        "lightgreen",
        "lightgrey",
        "lightpink",
        "lightsalmon",
        "lightseagreen",
        "lightskyblue",
        "lightslategray",
        "lightslategrey",
        "lightsteelblue",
        "lightyellow",
        "lime",
        "limegreen",
        "linen",
        "magenta",
        "maroon",
        "mediumaquamarine",
        "mediumblue",
        "mediumorchid",
        "mediumpurple",
        "mediumseagreen",
        "mediumslateblue",
        "mediumspringgreen",
        "mediumturquoise",
        "mediumvioletred",
        "midnightblue",
        "mintcream",
        "mistyrose",
        "moccasin",
        "navajowhite",
        "navy",
        "oldlace",
        "olive",
        "olivedrab",
        "orange",
        "orangered",
        "orchid",
        "palegoldenrod",
        "palegreen",
        "paleturquoise",
        "palevioletred",
        "papayawhip",
        "peachpuff",
        "peru",
        "pink",
        "plum",
        "powderblue",
        "purple",
        "red",
        "rosybrown",
        "royalblue",
        "saddlebrown",
        "salmon",
        "sandybrown",
        "seagreen",
        "seashell",
        "sienna",
        "silver",
        "skyblue",
        "slateblue",
        "slategray",
        "slategrey",
        "snow",
        "springgreen",
        "steelblue",
        "tan",
        "teal",
        "thistle",
        "tomato",
        "turquoise",
        "violet",
        "wheat",
        "white",
        "whitesmoke",
        "yellow",
        "yellowgreen",
    )
}

fn default_true() -> bool {
    true
}

fn default_include_in_availability() -> String {
    "all".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(color: &str) -> (Option<String>, Option<String>) {
        let mut calendar: Calendar =
            serde_json::from_value(serde_json::json!({ "name": "C", "color": color }))
                .expect("calendar");
        let dropped = calendar.normalize_color();
        (calendar.color, dropped)
    }

    // Alpha is stripped, valid colors pass through, anything else is dropped.
    #[test]
    fn calendar_colors_are_normalized_for_jmap() {
        for (input, expected) in [
            ("#AAAAAAFF", "#AAAAAA"),
            ("#3366ccfe", "#3366cc"),
            ("#ABCD", "#ABC"),
            ("#abc", "#abc"),
            ("#3366CC", "#3366CC"),
            (" #3366cc\n", "#3366cc"),
            ("CornflowerBlue", "CornflowerBlue"),
            ("grey", "grey"),
        ] {
            assert_eq!(
                normalized(input),
                (Some(expected.to_owned()), None),
                "{input:?}"
            );
        }
        for input in [
            "",
            "#",
            "#12345",
            "#GGGGGG",
            "#1234567",
            "rgb(1,2,3)",
            "notacolor",
        ] {
            assert_eq!(
                normalized(input),
                (None, Some(input.to_owned())),
                "{input:?}"
            );
        }
        let mut calendar: Calendar =
            serde_json::from_value(serde_json::json!({ "name": "C" })).expect("calendar");
        assert_eq!(calendar.normalize_color(), None);
        assert_eq!(calendar.color, None);
    }
}
