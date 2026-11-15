/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use serde_json::{Map, Value, json};

use crate::exchange::date::WireDate;
use crate::exchange::graph::error::GraphError;

#[derive(Debug, Clone)]
pub struct ConvertedContact {
    pub uid: String,
    pub data: Value,
}

const SYNTHETIC_UID_PREFIX: &str = "vandelay-graph-";

#[derive(Debug, Clone, Copy)]
pub struct GraphContact<'a>(pub &'a Value);

impl GraphContact<'_> {
    pub fn convert(self) -> Result<ConvertedContact, GraphError> {
        let graph_contact = self.0;
        let graph_id = graph_contact
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| GraphError::Malformed("contact has no id".to_owned()))?;
        let uid = GraphContact::synthetic_uid(graph_id);

        let mut card = Map::new();
        card.insert("@type".to_owned(), Value::from("Card"));
        card.insert("version".to_owned(), Value::from("1.0"));
        card.insert("uid".to_owned(), Value::from(uid.clone()));
        card.insert("kind".to_owned(), Value::from("individual"));

        let mut name_object = Map::new();
        name_object.insert("@type".to_owned(), Value::from("Name"));
        if let Some(display) = graph_contact.get("displayName").and_then(Value::as_str)
            && !display.is_empty()
        {
            name_object.insert("full".to_owned(), Value::from(display.to_owned()));
        }
        let name_slots = [
            ("title", graph_contact.get("title").and_then(Value::as_str)),
            (
                "given",
                graph_contact.get("givenName").and_then(Value::as_str),
            ),
            (
                "given2",
                graph_contact.get("middleName").and_then(Value::as_str),
            ),
            (
                "surname",
                graph_contact.get("surname").and_then(Value::as_str),
            ),
            (
                "generation",
                graph_contact.get("generation").and_then(Value::as_str),
            ),
        ];
        let mut components: Vec<Value> = Vec::new();
        let mut indices = [None; 5];
        for ((kind, value), index) in name_slots.into_iter().zip(indices.iter_mut()) {
            let Some(v) = value else { continue };
            if v.is_empty() {
                continue;
            }
            *index = Some(components.len());
            components.push(json!({"@type": "NameComponent", "kind": kind, "value": v}));
        }
        let [_, given_index, _, surname_index, _] = indices;
        if !components.is_empty() {
            name_object.insert("components".to_owned(), Value::Array(components));
        }
        if name_object.contains_key("full") || name_object.contains_key("components") {
            card.insert("name".to_owned(), Value::Object(name_object));
        }

        if let Some(nick) = graph_contact.get("nickName").and_then(Value::as_str)
            && !nick.is_empty()
        {
            let mut nicks = Map::new();
            nicks.insert(
                "nick-1".to_owned(),
                json!({"@type": "Nickname", "name": nick}),
            );
            card.insert("nicknames".to_owned(), Value::Object(nicks));
        }

        let mut emails = Map::new();
        if let Some(addrs) = graph_contact
            .get("emailAddresses")
            .and_then(Value::as_array)
        {
            for (i, e) in addrs.iter().enumerate() {
                let Some(addr) = e.get("address").and_then(Value::as_str) else {
                    continue;
                };
                if addr.is_empty() {
                    continue;
                }
                let key = format!("email-{}", i + 1);
                let mut entry = Map::new();
                entry.insert("@type".to_owned(), Value::from("EmailAddress"));
                entry.insert("address".to_owned(), Value::from(addr.to_owned()));
                if i == 0 {
                    entry.insert("pref".to_owned(), Value::from(1));
                }
                emails.insert(key, Value::Object(entry));
            }
        }
        if !emails.is_empty() {
            card.insert("emails".to_owned(), Value::Object(emails));
        }

        let mut phones = Map::new();
        GraphContact::push_phones(
            &mut phones,
            graph_contact
                .get("businessPhones")
                .and_then(Value::as_array),
            "work",
        );
        GraphContact::push_phones(
            &mut phones,
            graph_contact.get("homePhones").and_then(Value::as_array),
            "private",
        );
        if let Some(mobile) = graph_contact.get("mobilePhone").and_then(Value::as_str)
            && !mobile.is_empty()
        {
            let key = format!("phone-{}", phones.len() + 1);
            phones.insert(
                key,
                json!({
                    "@type": "Phone",
                    "number": mobile,
                    "features": {"mobile": true, "voice": true}
                }),
            );
        }
        if !phones.is_empty() {
            card.insert("phones".to_owned(), Value::Object(phones));
        }

        let mut addresses = Map::new();
        GraphContact::push_address(
            &mut addresses,
            graph_contact.get("homeAddress"),
            Some("private"),
        );
        GraphContact::push_address(
            &mut addresses,
            graph_contact.get("businessAddress"),
            Some("work"),
        );
        GraphContact::push_address(&mut addresses, graph_contact.get("otherAddress"), None);
        if !addresses.is_empty() {
            card.insert("addresses".to_owned(), Value::Object(addresses));
        }

        let mut organizations = Map::new();
        let has_company = graph_contact
            .get("companyName")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let has_dept = graph_contact
            .get("department")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let has_office = graph_contact
            .get("officeLocation")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let has_title = graph_contact
            .get("jobTitle")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if has_company || has_dept || has_office {
            let mut org = Map::new();
            org.insert("@type".to_owned(), Value::from("Organization"));
            if let Some(name) = graph_contact.get("companyName").and_then(Value::as_str)
                && !name.is_empty()
            {
                org.insert("name".to_owned(), Value::from(name.to_owned()));
            }
            let mut units: Vec<Value> = Vec::new();
            if let Some(dept) = graph_contact.get("department").and_then(Value::as_str)
                && !dept.is_empty()
            {
                units.push(json!({"@type": "OrgUnit", "name": dept}));
            }
            if let Some(office) = graph_contact.get("officeLocation").and_then(Value::as_str)
                && !office.is_empty()
            {
                units.push(json!({"@type": "OrgUnit", "name": office}));
            }
            if !units.is_empty() {
                org.insert("units".to_owned(), Value::Array(units));
            }
            organizations.insert("org-1".to_owned(), Value::Object(org));
        }
        if has_title
            && let Some(title) = graph_contact.get("jobTitle").and_then(Value::as_str)
            && !title.is_empty()
        {
            let mut titles = Map::new();
            let mut title_obj = Map::new();
            title_obj.insert("@type".to_owned(), Value::from("Title"));
            title_obj.insert("name".to_owned(), Value::from(title.to_owned()));
            if organizations.contains_key("org-1") {
                title_obj.insert("organizationId".to_owned(), Value::from("org-1"));
            }
            titles.insert("title-1".to_owned(), Value::Object(title_obj));
            card.insert("titles".to_owned(), Value::Object(titles));
        }
        let has_org_one = organizations.contains_key("org-1");
        if !organizations.is_empty() {
            card.insert("organizations".to_owned(), Value::Object(organizations));
        }

        if let Some(notes) = graph_contact.get("personalNotes").and_then(Value::as_str)
            && !notes.is_empty()
        {
            let mut map = Map::new();
            map.insert("note-1".to_owned(), json!({"@type": "Note", "note": notes}));
            card.insert("notes".to_owned(), Value::Object(map));
        }

        let mut localizations = Map::new();
        let mut ja = Map::new();
        if let Some(name) = graph_contact.get("yomiCompanyName").and_then(Value::as_str)
            && !name.is_empty()
            && has_org_one
        {
            ja.insert(
                "organizations/org-1/name".to_owned(),
                Value::from(name.to_owned()),
            );
        }
        if let Some(name) = graph_contact.get("yomiGivenName").and_then(Value::as_str)
            && !name.is_empty()
            && let Some(idx) = given_index
        {
            ja.insert(
                format!("name/components/{idx}/value"),
                Value::from(name.to_owned()),
            );
        }
        if let Some(name) = graph_contact.get("yomiSurname").and_then(Value::as_str)
            && !name.is_empty()
            && let Some(idx) = surname_index
        {
            ja.insert(
                format!("name/components/{idx}/value"),
                Value::from(name.to_owned()),
            );
        }
        if !ja.is_empty() {
            localizations.insert("ja".to_owned(), Value::Object(ja));
        }
        if !localizations.is_empty() {
            card.insert("localizations".to_owned(), Value::Object(localizations));
        }

        if let Some(birthday) = graph_contact.get("birthday").and_then(Value::as_str)
            && let Some(date) = WireDate(birthday).partial_date()
        {
            let mut anniversaries = Map::new();
            anniversaries.insert(
                "anniversary-1".to_owned(),
                json!({
                    "@type": "Anniversary",
                    "kind": "birth",
                    "date": date,
                }),
            );
            card.insert("anniversaries".to_owned(), Value::Object(anniversaries));
        }

        if let Some(link) = graph_contact
            .get("businessHomePage")
            .and_then(Value::as_str)
            && !link.is_empty()
        {
            let mut links = Map::new();
            links.insert(
                "link-1".to_owned(),
                json!({"@type": "Link", "uri": link, "contexts": {"work": true}}),
            );
            card.insert("links".to_owned(), Value::Object(links));
        }

        if let Some(cats) = graph_contact.get("categories").and_then(Value::as_array) {
            let mut keywords = Map::new();
            for cat in cats.iter().filter_map(Value::as_str) {
                if !cat.trim().is_empty() {
                    keywords.insert(cat.to_ascii_lowercase(), Value::Bool(true));
                }
            }
            if !keywords.is_empty() {
                card.insert("keywords".to_owned(), Value::Object(keywords));
            }
        }

        if let Some(ims) = graph_contact.get("imAddresses").and_then(Value::as_array) {
            let mut services = Map::new();
            for (i, addr) in ims
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .enumerate()
            {
                let slot = if GraphContact::has_uri_scheme(addr) {
                    "uri"
                } else {
                    "user"
                };
                services.insert(
                    (i + 1).to_string(),
                    json!({"@type": "OnlineService", slot: addr}),
                );
            }
            if !services.is_empty() {
                card.insert("onlineServices".to_owned(), Value::Object(services));
            }
        }

        if let Some(created) = graph_contact.get("createdDateTime").and_then(Value::as_str) {
            card.insert(
                "created".to_owned(),
                Value::from(WireDate(created).without_fraction()),
            );
        }
        if let Some(updated) = graph_contact
            .get("lastModifiedDateTime")
            .and_then(Value::as_str)
        {
            card.insert(
                "updated".to_owned(),
                Value::from(WireDate(updated).without_fraction()),
            );
        }

        Ok(ConvertedContact {
            uid,
            data: Value::Object(card),
        })
    }

    fn has_uri_scheme(value: &str) -> bool {
        let Some((scheme, rest)) = value.split_once(':') else {
            return false;
        };
        !rest.is_empty()
            && !scheme.is_empty()
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    }

    pub fn synthetic_uid(graph_id: &str) -> String {
        let hash = blake3::hash(graph_id.as_bytes());
        format!("{SYNTHETIC_UID_PREFIX}{}", hash.to_hex())
    }

    fn push_phones(target: &mut Map<String, Value>, list: Option<&Vec<Value>>, context: &str) {
        let Some(list) = list else { return };
        for n in list.iter().filter_map(Value::as_str) {
            if n.is_empty() {
                continue;
            }
            let key = format!("phone-{}", target.len() + 1);
            target.insert(
                key,
                json!({
                    "@type": "Phone",
                    "number": n,
                    "contexts": {context: true}
                }),
            );
        }
    }

    fn push_address(
        target: &mut Map<String, Value>,
        source: Option<&Value>,
        context: Option<&str>,
    ) {
        let Some(source) = source else { return };
        let Some(map) = source.as_object() else {
            return;
        };
        let street = map
            .get("street")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let component_fields: Vec<(&str, &str)> = [
            ("city", "locality"),
            ("state", "region"),
            ("postalCode", "postcode"),
            ("countryOrRegion", "country"),
        ]
        .into_iter()
        .filter_map(|(graph_key, jsc_kind)| {
            map.get(graph_key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| (jsc_kind, s))
        })
        .collect();
        if street.is_none() && component_fields.is_empty() {
            return;
        }
        let mut address = Map::new();
        address.insert("@type".to_owned(), Value::from("Address"));
        if let Some(context) = context {
            address.insert("contexts".to_owned(), json!({context: true}));
        }
        if let Some(s) = street {
            address.insert("full".to_owned(), Value::from(s.to_owned()));
        }
        if !component_fields.is_empty() {
            let components: Vec<Value> = component_fields
                .iter()
                .map(|(kind, value)| json!({"@type": "AddressComponent", "kind": kind, "value": value}))
                .collect();
            address.insert("components".to_owned(), Value::Array(components));
        }
        let key = format!("addr-{}", target.len() + 1);
        target.insert(key, Value::Object(address));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_contact_yields_card_with_synthetic_uid() {
        let c = json!({"id": "GRAPH-1", "displayName": "Alice"});
        let conv = GraphContact(&c).convert().unwrap();
        assert!(conv.uid.starts_with("vandelay-graph-"));
        assert_eq!(conv.data["@type"], "Card");
        assert_eq!(conv.data["kind"], "individual");
        assert_eq!(conv.data["name"]["full"], "Alice");
    }

    #[test]
    fn synthetic_uid_is_stable_per_graph_id() {
        let a = GraphContact::synthetic_uid("AAA");
        let b = GraphContact::synthetic_uid("AAA");
        let c = GraphContact::synthetic_uid("BBB");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn emails_map_with_first_as_pref() {
        let c = json!({
            "id": "X",
            "emailAddresses": [
                {"address": "a@x.com", "name": "Alice"},
                {"address": "b@x.com", "name": "Alice B"}
            ]
        });
        let conv = GraphContact(&c).convert().unwrap();
        let emails = conv.data["emails"].as_object().unwrap();
        assert_eq!(emails["email-1"]["address"], "a@x.com");
        assert_eq!(emails["email-1"]["pref"], 1);
        assert_eq!(emails["email-2"]["address"], "b@x.com");
        assert!(emails["email-2"].get("pref").is_none());
    }

    #[test]
    fn phones_carry_context_per_source_field() {
        let c = json!({
            "id": "X",
            "businessPhones": ["+1-555-2000"],
            "homePhones": ["+1-555-1000"],
            "mobilePhone": "+1-555-3000"
        });
        let conv = GraphContact(&c).convert().unwrap();
        let phones = conv.data["phones"].as_object().unwrap();
        assert_eq!(phones.len(), 3);
        let has_work = phones
            .values()
            .any(|v| v["contexts"]["work"] == Value::Bool(true));
        let has_private = phones
            .values()
            .any(|v| v["contexts"]["private"] == Value::Bool(true));
        let has_mobile = phones
            .values()
            .any(|v| v["features"]["mobile"] == Value::Bool(true));
        assert!(has_work && has_private && has_mobile);
    }

    #[test]
    fn addresses_map_three_contexts() {
        let c = json!({
            "id": "X",
            "homeAddress": {"city": "Home City"},
            "businessAddress": {"city": "Work City"},
            "otherAddress": {"city": "Other City"}
        });
        let conv = GraphContact(&c).convert().unwrap();
        let addresses = conv.data["addresses"].as_object().unwrap();
        assert_eq!(addresses.len(), 3);
        let has_other_context = addresses
            .values()
            .any(|a| a.get("contexts").and_then(|c| c.get("other")).is_some());
        assert!(!has_other_context);
        let without_context = addresses
            .values()
            .filter(|a| a.get("contexts").is_none())
            .count();
        assert_eq!(without_context, 1);
    }

    #[test]
    fn org_and_title_pair_into_separate_jscontact_objects() {
        let c = json!({
            "id": "X",
            "companyName": "Acme",
            "department": "Engineering",
            "officeLocation": "Building 9",
            "jobTitle": "Architect"
        });
        let conv = GraphContact(&c).convert().unwrap();
        let orgs = conv.data["organizations"].as_object().unwrap();
        assert!(orgs.contains_key("org-1"));
        assert_eq!(orgs["org-1"]["name"], "Acme");
        let titles = conv.data["titles"].as_object().unwrap();
        assert_eq!(titles["title-1"]["name"], "Architect");
        assert_eq!(titles["title-1"]["organizationId"], "org-1");
    }

    #[test]
    fn yomi_fields_populate_japanese_localization() {
        let c = json!({
            "id": "X",
            "displayName": "Yamada Taro",
            "givenName": "Taro",
            "surname": "Yamada",
            "companyName": "Acme",
            "yomiCompanyName": "アクメ",
            "yomiGivenName": "タロウ",
            "yomiSurname": "ヤマダ"
        });
        let conv = GraphContact(&c).convert().unwrap();
        let ja = conv.data["localizations"]["ja"].as_object().unwrap();
        assert_eq!(ja["organizations/org-1/name"], "アクメ");
    }

    #[test]
    fn birthday_stored_as_local_midnight_in_utc_keeps_its_day() {
        for raw in ["1990-04-14T22:00:00Z", "1990-04-15T11:59:00Z", "1990-04-15"] {
            let conv = GraphContact(&json!({"id": "X", "birthday": raw}))
                .convert()
                .unwrap();
            assert_eq!(
                conv.data["anniversaries"]["anniversary-1"]["date"],
                json!({"@type": "PartialDate", "year": 1990, "month": 4, "day": 15})
            );
        }
    }

    #[test]
    fn outlook_no_date_birthday_is_dropped() {
        let c = json!({"id": "X", "birthday": "4501-01-01T00:00:00Z"});
        let conv = GraphContact(&c).convert().unwrap();
        assert!(conv.data.get("anniversaries").is_none());
    }

    #[test]
    fn birthday_emits_partialdate_object() {
        let c = json!({"id": "X", "birthday": "1980-04-12T00:00:00Z"});
        let conv = GraphContact(&c).convert().unwrap();
        let ann = conv.data["anniversaries"].as_object().unwrap();
        let date = &ann["anniversary-1"]["date"];
        assert_eq!(date["@type"], "PartialDate");
        assert_eq!(date["year"], 1980);
        assert_eq!(date["month"], 4);
        assert_eq!(date["day"], 12);
        assert_eq!(ann["anniversary-1"]["kind"], "birth");
    }

    #[test]
    fn mobile_phone_uses_features_not_contexts() {
        let c = json!({"id": "X", "mobilePhone": "+1-555-3000"});
        let conv = GraphContact(&c).convert().unwrap();
        let phones = conv.data["phones"].as_object().unwrap();
        let phone = phones.values().next().unwrap();
        assert_eq!(phone["features"]["mobile"], true);
        assert!(phone.get("contexts").is_none());
    }

    #[test]
    fn generation_maps_to_generation_kind() {
        let c = json!({"id": "X", "generation": "Jr."});
        let conv = GraphContact(&c).convert().unwrap();
        let comps = conv.data["name"]["components"].as_array().unwrap();
        assert!(
            comps
                .iter()
                .any(|c| c["kind"] == "generation" && c["value"] == "Jr.")
        );
    }

    #[test]
    fn only_jobtitle_does_not_emit_empty_organization() {
        let c = json!({"id": "X", "jobTitle": "Architect"});
        let conv = GraphContact(&c).convert().unwrap();
        assert!(conv.data.get("organizations").is_none());
        let titles = conv.data["titles"].as_object().unwrap();
        assert_eq!(titles["title-1"]["name"], "Architect");
        assert!(titles["title-1"].get("organizationId").is_none());
    }

    #[test]
    fn related_contacts_are_dropped_without_resolvable_uids() {
        let c = json!({
            "id": "X",
            "manager": "alice@x.com",
            "spouseName": "Bob",
            "children": ["Kid A", "Kid B"]
        });
        let conv = GraphContact(&c).convert().unwrap();
        assert!(conv.data.get("relatedTo").is_none());
    }

    #[test]
    fn nameless_contact_omits_name_object() {
        let c = json!({"id": "X", "businessPhones": ["+1"]});
        let conv = GraphContact(&c).convert().unwrap();
        assert!(conv.data.get("name").is_none());
    }

    #[test]
    fn timestamps_strip_fractional_seconds() {
        let c = json!({
            "id": "X",
            "displayName": "Alice",
            "createdDateTime": "2026-05-01T08:00:00.0000000Z",
            "lastModifiedDateTime": "2026-05-02T08:00:00.123Z"
        });
        let conv = GraphContact(&c).convert().unwrap();
        assert_eq!(conv.data["created"], "2026-05-01T08:00:00Z");
        assert_eq!(conv.data["updated"], "2026-05-02T08:00:00Z");
    }
}
