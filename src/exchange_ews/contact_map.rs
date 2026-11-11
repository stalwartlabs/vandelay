/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use chrono::Datelike;
use serde_json::{Map, Value, json};

use crate::exchange::date::{is_outlook_no_date, nearest_midnight};
use crate::exchange_ews::parse::{ContactItemRaw, RawContactAddress};

pub fn synthetic_uid(item_id: &str) -> String {
    let hash = blake3::hash(item_id.as_bytes()).to_hex();
    format!("vandelay-ews-syn-{hash}")
}

pub fn to_jscontact(raw: &ContactItemRaw) -> Value {
    let mut card = Map::new();
    card.insert("@type".to_owned(), Value::String("Card".to_owned()));
    card.insert("version".to_owned(), Value::String("1.0".to_owned()));
    if raw.is_group {
        card.insert("kind".to_owned(), Value::String("group".to_owned()));
        let mut members = Map::new();
        for m in &raw.members {
            let key = match m.email.as_ref() {
                Some(email) => format!("mailto:{email}"),
                None => match m.name.as_ref() {
                    Some(name) => name.clone(),
                    None => continue,
                },
            };
            members.insert(key, Value::Bool(true));
        }
        if !members.is_empty() {
            card.insert("members".to_owned(), Value::Object(members));
        }
    }
    if let Some(full) = raw.display_name.as_ref() {
        let mut name = Map::new();
        name.insert("@type".to_owned(), Value::String("Name".to_owned()));
        name.insert("full".to_owned(), Value::String(full.clone()));
        let components = name_components(raw);
        if !components.is_empty() {
            name.insert("components".to_owned(), Value::Array(components));
        }
        card.insert("name".to_owned(), Value::Object(name));
    }
    if let Some(n) = raw.nickname.as_ref() {
        card.insert(
            "nicknames".to_owned(),
            Value::Object(map_singleton("1", json!({"@type": "Nickname", "name": n}))),
        );
    }
    let mut organizations: Map<String, Value> = Map::new();
    let mut have_org = false;
    let mut org_inner: Map<String, Value> = Map::new();
    org_inner.insert("@type".to_owned(), Value::String("Organization".to_owned()));
    if let Some(co) = raw.company_name.as_ref() {
        org_inner.insert("name".to_owned(), Value::String(co.clone()));
        have_org = true;
    }
    let mut org_units: Vec<Value> = Vec::new();
    if let Some(dep) = raw.department.as_ref() {
        org_units.push(json!({"@type": "OrgUnit", "name": dep}));
    }
    if let Some(office) = raw.office_location.as_ref() {
        org_units.push(json!({"@type": "OrgUnit", "name": office}));
    }
    if !org_units.is_empty() {
        org_inner.insert("units".to_owned(), Value::Array(org_units));
        have_org = true;
    }
    if have_org {
        organizations.insert("1".to_owned(), Value::Object(org_inner));
    }
    let mut next_org_idx = 2u32;
    for extra in &raw.companies {
        if matches!(raw.company_name.as_deref(), Some(c) if c == extra) {
            continue;
        }
        organizations.insert(
            next_org_idx.to_string(),
            json!({"@type": "Organization", "name": extra}),
        );
        next_org_idx += 1;
    }
    if !organizations.is_empty() {
        card.insert("organizations".to_owned(), Value::Object(organizations));
    }
    if let Some(t) = raw.job_title.as_ref() {
        card.insert(
            "titles".to_owned(),
            Value::Object(map_singleton("1", json!({"@type": "Title", "name": t}))),
        );
    }
    {
        let mut map = Map::new();
        for (i, (key, addr)) in raw
            .emails
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .enumerate()
        {
            let mut entry = Map::new();
            entry.insert("@type".to_owned(), Value::String("EmailAddress".to_owned()));
            entry.insert("address".to_owned(), Value::String(addr.clone()));
            entry.insert("contexts".to_owned(), email_contexts(key));
            map.insert((i + 1).to_string(), Value::Object(entry));
        }
        if !map.is_empty() {
            card.insert("emails".to_owned(), Value::Object(map));
        }
    }
    {
        let mut map = Map::new();
        for (i, (key, num)) in raw
            .phones
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .enumerate()
        {
            let mut entry = Map::new();
            entry.insert("@type".to_owned(), Value::String("Phone".to_owned()));
            entry.insert("number".to_owned(), Value::String(num.clone()));
            entry.insert("contexts".to_owned(), phone_contexts(key));
            entry.insert("features".to_owned(), phone_features(key));
            map.insert((i + 1).to_string(), Value::Object(entry));
        }
        if !map.is_empty() {
            card.insert("phones".to_owned(), Value::Object(map));
        }
    }
    {
        let mut map = Map::new();
        for (i, (key, addr)) in raw
            .ims
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .enumerate()
        {
            let entry_id = (i + 1).to_string();
            let mut entry = Map::new();
            entry.insert(
                "@type".to_owned(),
                Value::String("OnlineService".to_owned()),
            );
            entry.insert("user".to_owned(), Value::String(addr.clone()));
            if let Some(service) = im_service_from_key(key) {
                entry.insert("service".to_owned(), Value::String(service));
            }
            map.insert(entry_id, Value::Object(entry));
        }
        if !map.is_empty() {
            card.insert("onlineServices".to_owned(), Value::Object(map));
        }
    }
    if !raw.addresses.is_empty() {
        let pref_key = raw
            .postal_address_index
            .as_deref()
            .map(str::to_ascii_lowercase);
        let mut map = Map::new();
        for (i, addr) in raw.addresses.iter().enumerate() {
            let entry_id = format!("{}", i + 1);
            let is_pref = matches!(&pref_key, Some(p) if !p.is_empty() && p == &addr.key.to_ascii_lowercase());
            map.insert(entry_id, address_to_jscontact(addr, is_pref));
        }
        card.insert("addresses".to_owned(), Value::Object(map));
    }
    if let Some(url) = raw.url.as_ref() {
        card.insert(
            "links".to_owned(),
            Value::Object(map_singleton("1", json!({"@type": "Link", "uri": url}))),
        );
    }
    if let Some(bday) = raw.birthday.as_ref()
        && let Some(partial) = partial_date(bday)
    {
        card.insert(
            "anniversaries".to_owned(),
            Value::Object(map_singleton(
                "birth",
                json!({"@type": "Anniversary", "kind": "birth", "date": partial}),
            )),
        );
    }
    if let Some(anniv) = raw.wedding_anniversary.as_ref()
        && let Some(partial) = partial_date(anniv)
    {
        let key = "wedding";
        let entry = json!({"@type": "Anniversary", "kind": "wedding", "date": partial});
        if let Some(map) = card.get_mut("anniversaries").and_then(Value::as_object_mut) {
            map.insert(key.to_owned(), entry);
        } else {
            card.insert(
                "anniversaries".to_owned(),
                Value::Object(map_singleton(key, entry)),
            );
        }
    }
    if !raw.categories.is_empty() {
        let mut keywords = Map::new();
        for cat in &raw.categories {
            keywords.insert(cat.to_ascii_lowercase(), Value::Bool(true));
        }
        card.insert("keywords".to_owned(), Value::Object(keywords));
    }
    if let Some(notes) = raw.notes.as_ref().filter(|n| !n.trim().is_empty()) {
        card.insert(
            "notes".to_owned(),
            Value::Object(map_singleton("1", json!({"@type": "Note", "note": notes}))),
        );
    }
    Value::Object(card)
}

fn name_components(raw: &ContactItemRaw) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if let Some(v) = raw.given_name.as_ref() {
        out.push(json!({"@type": "NameComponent", "kind": "given", "value": v}));
    }
    if let Some(v) = raw.middle_name.as_ref() {
        out.push(json!({"@type": "NameComponent", "kind": "given2", "value": v}));
    }
    if let Some(v) = raw.surname.as_ref() {
        out.push(json!({"@type": "NameComponent", "kind": "surname", "value": v}));
    }
    if let Some(v) = raw.generation.as_ref() {
        out.push(json!({"@type": "NameComponent", "kind": "generation", "value": v}));
    }
    out
}

fn email_contexts(key: &str) -> Value {
    let mut o = Map::new();
    let lower = key.to_ascii_lowercase();
    if lower.contains("business") || lower.contains("work") {
        o.insert("work".to_owned(), Value::Bool(true));
    } else if lower.contains("home") {
        o.insert("private".to_owned(), Value::Bool(true));
    }
    Value::Object(o)
}

fn phone_contexts(key: &str) -> Value {
    let mut o = Map::new();
    let lower = key.to_ascii_lowercase();
    if lower.starts_with("business") || lower.starts_with("company") {
        o.insert("work".to_owned(), Value::Bool(true));
    } else if lower.starts_with("home") || lower.starts_with("mobile") {
        o.insert("private".to_owned(), Value::Bool(true));
    }
    Value::Object(o)
}

fn phone_features(key: &str) -> Value {
    let mut o = Map::new();
    let lower = key.to_ascii_lowercase();
    if lower.contains("mobile") {
        o.insert("mobile".to_owned(), Value::Bool(true));
    } else if lower.contains("fax") {
        o.insert("fax".to_owned(), Value::Bool(true));
    } else if lower.contains("pager") {
        o.insert("pager".to_owned(), Value::Bool(true));
    } else {
        o.insert("voice".to_owned(), Value::Bool(true));
    }
    Value::Object(o)
}

fn address_to_jscontact(addr: &RawContactAddress, is_pref: bool) -> Value {
    let mut o = Map::new();
    o.insert("@type".to_owned(), Value::String("Address".to_owned()));
    let mut components: Vec<Value> = Vec::new();
    if let Some(s) = addr.street.as_ref() {
        components.push(json!({"@type": "AddressComponent", "kind": "name", "value": s}));
    }
    if let Some(s) = addr.city.as_ref() {
        components.push(json!({"@type": "AddressComponent", "kind": "locality", "value": s}));
    }
    if let Some(s) = addr.state.as_ref() {
        components.push(json!({"@type": "AddressComponent", "kind": "region", "value": s}));
    }
    if let Some(s) = addr.postal_code.as_ref() {
        components.push(json!({"@type": "AddressComponent", "kind": "postcode", "value": s}));
    }
    if let Some(s) = addr.country.as_ref() {
        components.push(json!({"@type": "AddressComponent", "kind": "country", "value": s}));
    }
    if !components.is_empty() {
        o.insert("components".to_owned(), Value::Array(components));
    }
    let mut contexts = Map::new();
    let lower = addr.key.to_ascii_lowercase();
    if lower == "business" || lower == "work" {
        contexts.insert("work".to_owned(), Value::Bool(true));
    } else if lower == "home" {
        contexts.insert("private".to_owned(), Value::Bool(true));
    }
    if !contexts.is_empty() {
        o.insert("contexts".to_owned(), Value::Object(contexts));
    }
    if is_pref {
        o.insert("pref".to_owned(), Value::from(1u32));
    }
    Value::Object(o)
}

fn im_service_from_key(key: &str) -> Option<String> {
    let trimmed: String = key.chars().filter(|c| !c.is_ascii_digit()).collect();
    let canonical = trimmed.trim_start_matches("ImAddress");
    if canonical.is_empty() {
        return None;
    }
    Some(canonical.to_owned())
}

fn partial_date(s: &str) -> Option<Value> {
    if is_outlook_no_date(s) {
        return None;
    }
    let date = nearest_midnight(s, None)?;
    let mut o = Map::new();
    o.insert("@type".to_owned(), Value::String("PartialDate".to_owned()));
    if date.year() > 0 {
        o.insert("year".to_owned(), Value::from(date.year()));
    }
    o.insert("month".to_owned(), Value::from(date.month()));
    o.insert("day".to_owned(), Value::from(date.day()));
    Some(Value::Object(o))
}

fn map_singleton(key: &str, value: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(key.to_owned(), value);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_contact_round_trip() {
        let raw = ContactItemRaw {
            display_name: Some("Alice Doe".to_owned()),
            given_name: Some("Alice".to_owned()),
            surname: Some("Doe".to_owned()),
            emails: vec![("EmailAddress1".to_owned(), "alice@x".to_owned())],
            phones: vec![("BusinessPhone1".to_owned(), "+1".to_owned())],
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert_eq!(v["@type"], "Card");
        assert_eq!(v["name"]["full"], "Alice Doe");
        let comps = v["name"]["components"].as_array().unwrap();
        assert_eq!(comps[0]["kind"], "given");
        assert_eq!(comps[0]["value"], "Alice");
        assert_eq!(v["emails"]["1"]["address"], "alice@x");
        assert_eq!(v["phones"]["1"]["contexts"]["work"], true);
    }

    #[test]
    fn empty_body_and_empty_im_entry_are_omitted() {
        let raw = ContactItemRaw {
            display_name: Some("Random Person".to_owned()),
            notes: Some(String::new()),
            ims: vec![("ImAddress1".to_owned(), String::new())],
            emails: vec![("EmailAddress1".to_owned(), "  ".to_owned())],
            phones: vec![("MobilePhone".to_owned(), "+17801234567".to_owned())],
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert!(v.get("notes").is_none(), "empty body must not emit notes");
        assert!(
            v.get("onlineServices").is_none(),
            "empty IM entry must not emit onlineServices"
        );
        assert!(
            v.get("emails").is_none(),
            "whitespace-only email must not emit emails"
        );
        assert_eq!(v["phones"]["1"]["number"], "+17801234567");
    }

    #[test]
    fn synthetic_uid_is_stable() {
        let u1 = synthetic_uid("ITEM-1");
        let u2 = synthetic_uid("ITEM-1");
        assert_eq!(u1, u2);
        assert_ne!(u1, synthetic_uid("ITEM-2"));
        assert!(u1.starts_with("vandelay-ews-syn-"));
    }

    #[test]
    fn anniversaries_combine_birth_and_wedding() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            birthday: Some("1990-04-15T00:00:00Z".to_owned()),
            wedding_anniversary: Some("2015-08-22T00:00:00".to_owned()),
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert_eq!(v["anniversaries"]["birth"]["kind"], "birth");
        assert_eq!(v["anniversaries"]["birth"]["date"]["month"], 4);
        assert_eq!(v["anniversaries"]["wedding"]["kind"], "wedding");
    }

    #[test]
    fn birthday_stored_as_local_midnight_in_utc_keeps_its_day() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            birthday: Some("1990-04-14T22:00:00Z".to_owned()),
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert_eq!(
            v["anniversaries"]["birth"]["date"],
            json!({"@type": "PartialDate", "year": 1990, "month": 4, "day": 15})
        );
    }

    #[test]
    fn year_zero_birthday_has_no_year() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            birthday: Some("0000-04-15T00:00:00Z".to_owned()),
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert_eq!(
            v["anniversaries"]["birth"]["date"],
            json!({"@type": "PartialDate", "month": 4, "day": 15})
        );
    }

    #[test]
    fn outlook_no_date_anniversary_is_dropped() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            birthday: Some("1990-04-15T00:00:00Z".to_owned()),
            wedding_anniversary: Some("4501-01-01T00:00:00Z".to_owned()),
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert!(v["anniversaries"].get("wedding").is_none());
        assert_eq!(v["anniversaries"]["birth"]["date"]["day"], 15);
    }

    #[test]
    fn office_location_is_added_as_org_unit() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            company_name: Some("Initech".to_owned()),
            department: Some("R&D".to_owned()),
            office_location: Some("Bldg 4".to_owned()),
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        let units = v["organizations"]["1"]["units"].as_array().unwrap();
        assert_eq!(units[0]["name"], "R&D");
        assert_eq!(units[1]["name"], "Bldg 4");
    }

    #[test]
    fn extra_companies_become_additional_organizations() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            company_name: Some("Initech".to_owned()),
            companies: vec!["Initech".to_owned(), "Initrode".to_owned()],
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert_eq!(v["organizations"]["1"]["name"], "Initech");
        assert_eq!(v["organizations"]["2"]["name"], "Initrode");
    }

    #[test]
    fn postal_address_index_sets_pref_on_matching_entry() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            addresses: vec![
                RawContactAddress {
                    key: "Home".to_owned(),
                    street: Some("1 St".to_owned()),
                    ..Default::default()
                },
                RawContactAddress {
                    key: "Business".to_owned(),
                    street: Some("2 Av".to_owned()),
                    ..Default::default()
                },
            ],
            postal_address_index: Some("Business".to_owned()),
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert!(v["addresses"]["1"].get("pref").is_none());
        assert_eq!(v["addresses"]["2"]["pref"], 1);
    }

    #[test]
    fn im_service_extracted_from_key() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            ims: vec![("ImAddress1".to_owned(), "alice@msn".to_owned())],
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert_eq!(v["onlineServices"]["1"]["user"], "alice@msn");
        assert!(v["onlineServices"]["1"].get("service").is_none());
    }

    #[test]
    fn distribution_list_becomes_group_card_with_members() {
        let raw = ContactItemRaw {
            display_name: Some("The Team".to_owned()),
            is_group: true,
            members: vec![
                crate::exchange_ews::parse::RawGroupMember {
                    name: Some("Bob".to_owned()),
                    email: Some("bob@x".to_owned()),
                },
                crate::exchange_ews::parse::RawGroupMember {
                    name: Some("No Email".to_owned()),
                    email: None,
                },
            ],
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert_eq!(v["kind"], "group");
        assert_eq!(v["name"]["full"], "The Team");
        assert_eq!(v["members"]["mailto:bob@x"], true);
        assert_eq!(v["members"]["No Email"], true);
    }

    #[test]
    fn individual_contact_has_no_kind_or_members() {
        let raw = ContactItemRaw {
            display_name: Some("Alice".to_owned()),
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert!(v.get("kind").is_none());
        assert!(v.get("members").is_none());
    }

    #[test]
    fn unresolvable_relations_are_dropped() {
        let raw = ContactItemRaw {
            display_name: Some("X".to_owned()),
            manager: Some("Boss".to_owned()),
            assistant: Some("Asst".to_owned()),
            spouse: Some("Partner".to_owned()),
            children: vec!["Kid".to_owned()],
            ..ContactItemRaw::default()
        };
        let v = to_jscontact(&raw);
        assert!(v.get("relatedTo").is_none());
    }
}
