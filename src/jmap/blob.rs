/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};

use crate::jmap::error::JmapError;

pub const SENTINEL_KEY: &str = "@blob";

const DEFAULT_MEDIA_TYPE: &str = "application/octet-stream";
const MEDIA_TYPE_KEYS: [&str; 2] = ["mediaType", "contentType"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineShape {
    JsContactResource,
    JsCalendarLink,
}

impl InlineShape {
    fn uri_key(self) -> &'static str {
        match self {
            InlineShape::JsContactResource => "uri",
            InlineShape::JsCalendarLink => "href",
        }
    }
}

pub fn import_blob_ids<F>(value: &mut Value, mut resolve: F) -> Result<(), BlobWalkError>
where
    F: FnMut(&str) -> Result<i64, BlobWalkError>,
{
    walk_import(value, &mut resolve)
}

fn walk_import<F>(value: &mut Value, resolve: &mut F) -> Result<(), BlobWalkError>
where
    F: FnMut(&str) -> Result<i64, BlobWalkError>,
{
    match value {
        Value::Object(map) => {
            if let Some(Value::String(blob_id)) = map.remove("blobId") {
                let local = resolve(&blob_id)?;
                map.insert(SENTINEL_KEY.to_owned(), Value::from(local));
            }
            for child in map.values_mut() {
                walk_import(child, resolve)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_import(item, resolve)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn inline_blob_data_uris<F>(
    value: &mut Value,
    shape: InlineShape,
    mut fetch: F,
) -> Result<(), BlobWalkError>
where
    F: FnMut(i64) -> Result<Vec<u8>, BlobWalkError>,
{
    walk_inline(value, shape, &mut fetch)
}

fn walk_inline<F>(value: &mut Value, shape: InlineShape, fetch: &mut F) -> Result<(), BlobWalkError>
where
    F: FnMut(i64) -> Result<Vec<u8>, BlobWalkError>,
{
    match value {
        Value::Object(map) => {
            if let Some(sentinel) = map.remove(SENTINEL_KEY) {
                let local_id = sentinel.as_i64().ok_or(BlobWalkError::MalformedSentinel)?;
                let bytes = fetch(local_id)?;
                let uri = data_uri(map, &bytes);
                map.insert(shape.uri_key().to_owned(), Value::String(uri));
            }
            for child in map.values_mut() {
                walk_inline(child, shape, fetch)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_inline(item, shape, fetch)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn data_uri(map: &Map<String, Value>, bytes: &[u8]) -> String {
    let media_type = MEDIA_TYPE_KEYS
        .iter()
        .find_map(|k| map.get(*k).and_then(Value::as_str).and_then(uri_media_type))
        .unwrap_or_else(|| DEFAULT_MEDIA_TYPE.to_owned());
    let mut uri = String::with_capacity(13 + media_type.len() + bytes.len().div_ceil(3) * 4);
    uri.push_str("data:");
    uri.push_str(&media_type);
    uri.push_str(";base64,");
    STANDARD.encode_string(bytes, &mut uri);
    uri
}

fn uri_media_type(raw: &str) -> Option<String> {
    let mut parts = raw.split(';').map(str::trim);
    let essence = parts.next()?;
    let (ty, subtype) = essence.split_once('/')?;
    if ty.is_empty() || subtype.is_empty() || subtype.contains('/') {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    escape_into(&mut out, essence);
    for parameter in parts.filter(|p| !p.is_empty()) {
        out.push(';');
        escape_into(&mut out, parameter);
    }
    Some(out)
}

fn escape_into(out: &mut String, part: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in part.bytes() {
        if is_media_type_uri_byte(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0F) as usize] as char);
        }
    }
}

fn is_media_type_uri_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'/'
                | b':'
                | b'='
                | b'@'
                | b'_'
                | b'~'
        )
}

pub fn prepend_property(target: &mut Map<String, Value>, key: &str, value: Value) {
    let existing: Vec<(String, Value)> =
        target.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    target.clear();
    target.insert(key.to_owned(), value);
    for (k, v) in existing {
        if k != key {
            target.insert(k, v);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlobWalkError {
    #[error("malformed @blob sentinel: expected integer local id")]
    MalformedSentinel,
    #[error("blob resolver failed: {0}")]
    Resolver(Box<JmapError>),
}

impl BlobWalkError {
    pub fn resolver(source: JmapError) -> BlobWalkError {
        BlobWalkError::Resolver(Box::new(source))
    }

    pub fn into_source(self) -> JmapError {
        match self {
            BlobWalkError::Resolver(source) => *source,
            other => JmapError::Blob(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fetch_ok(local_id: i64) -> Result<Vec<u8>, BlobWalkError> {
        Ok(format!("bytes-{local_id}").into_bytes())
    }

    fn decode(uri: &str, expect_media_type: &str) -> Vec<u8> {
        assert_well_formed(uri);
        let prefix = format!("data:{expect_media_type};base64,");
        let payload = uri
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("{uri} does not start with {prefix}"));
        STANDARD.decode(payload).expect("base64 payload")
    }

    fn assert_well_formed(uri: &str) {
        let rest = uri
            .strip_prefix("data:")
            .unwrap_or_else(|| panic!("{uri} is not a data URI"));
        let (media_type, payload) = rest
            .split_once(";base64,")
            .unwrap_or_else(|| panic!("{uri} carries no ;base64, separator"));
        let bytes = media_type.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'%' {
                assert!(
                    i + 2 < bytes.len()
                        && bytes[i + 1].is_ascii_hexdigit()
                        && bytes[i + 2].is_ascii_hexdigit(),
                    "truncated percent escape in {uri}"
                );
                i += 3;
                continue;
            }
            assert!(
                is_media_type_uri_byte(b) || b == b';',
                "byte {b:#04x} is not allowed unescaped in {uri}"
            );
            i += 1;
        }
        assert!(
            payload
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
            "non-base64 payload in {uri}"
        );
    }

    fn assert_no_blob_id(value: &Value) {
        match value {
            Value::Object(map) => {
                assert!(map.get("blobId").is_none(), "blobId survived in {value}");
                assert!(map.get(SENTINEL_KEY).is_none(), "@blob survived in {value}");
                for child in map.values() {
                    assert_no_blob_id(child);
                }
            }
            Value::Array(items) => items.iter().for_each(assert_no_blob_id),
            _ => {}
        }
    }

    #[test]
    fn media_sentinel_becomes_a_uri_property() {
        let mut card = json!({
            "@type": "Card",
            "media": { "photo": {
                "@type": "Media", "kind": "photo",
                "@blob": 7, "mediaType": "image/png"
            } }
        });
        inline_blob_data_uris(&mut card, InlineShape::JsContactResource, fetch_ok).unwrap();
        let photo = &card["media"]["photo"];
        assert_eq!(
            decode(photo["uri"].as_str().unwrap(), "image/png"),
            b"bytes-7"
        );
        assert_eq!(photo["mediaType"], json!("image/png"));
        assert!(photo.get("href").is_none());
        assert_no_blob_id(&card);
    }

    #[test]
    fn calendar_link_sentinel_becomes_an_href_property() {
        let mut event = json!({
            "@type": "Event",
            "links": { "1": {
                "@type": "Link", "rel": "enclosure",
                "@blob": 3, "contentType": "text/plain", "title": "note.txt"
            } }
        });
        inline_blob_data_uris(&mut event, InlineShape::JsCalendarLink, fetch_ok).unwrap();
        let link = &event["links"]["1"];
        assert_eq!(
            decode(link["href"].as_str().unwrap(), "text/plain"),
            b"bytes-3"
        );
        assert_eq!(link["contentType"], json!("text/plain"));
        assert_eq!(link["title"], json!("note.txt"));
        assert!(link.get("uri").is_none());
        assert_no_blob_id(&event);
    }

    #[test]
    fn jscontact_link_keeps_the_resource_uri_property() {
        let mut card = json!({
            "@type": "Card",
            "links": { "l1": { "@type": "Link", "@blob": 1, "mediaType": "application/pdf" } }
        });
        inline_blob_data_uris(&mut card, InlineShape::JsContactResource, fetch_ok).unwrap();
        let link = &card["links"]["l1"];
        assert!(link.get("href").is_none(), "JSContact Link uses uri");
        assert_eq!(
            decode(link["uri"].as_str().unwrap(), "application/pdf"),
            b"bytes-1"
        );
    }

    #[test]
    fn absent_media_type_defaults_to_octet_stream() {
        let mut card = json!({ "media": { "photo": { "@blob": 2 } } });
        inline_blob_data_uris(&mut card, InlineShape::JsContactResource, fetch_ok).unwrap();
        let uri = card["media"]["photo"]["uri"].as_str().unwrap();
        assert_eq!(decode(uri, DEFAULT_MEDIA_TYPE), b"bytes-2");
    }

    #[test]
    fn empty_media_type_defaults_to_octet_stream() {
        let mut event = json!({ "links": { "1": { "@blob": 4, "contentType": "" } } });
        inline_blob_data_uris(&mut event, InlineShape::JsCalendarLink, fetch_ok).unwrap();
        let uri = event["links"]["1"]["href"].as_str().unwrap();
        assert_eq!(decode(uri, DEFAULT_MEDIA_TYPE), b"bytes-4");
    }

    #[test]
    fn empty_media_type_falls_through_to_the_next_key() {
        let mut card = json!({ "media": { "photo": {
            "@blob": 4, "mediaType": "", "contentType": "image/jpeg"
        } } });
        inline_blob_data_uris(&mut card, InlineShape::JsContactResource, fetch_ok).unwrap();
        let uri = card["media"]["photo"]["uri"].as_str().unwrap();
        assert_eq!(decode(uri, "image/jpeg"), b"bytes-4");
    }

    #[test]
    fn media_type_parameters_are_kept_without_whitespace() {
        let mut event = json!({ "links": { "1": {
            "@blob": 4, "contentType": "text/plain; charset=utf-8"
        } } });
        inline_blob_data_uris(&mut event, InlineShape::JsCalendarLink, fetch_ok).unwrap();
        let uri = event["links"]["1"]["href"].as_str().unwrap();
        assert!(!uri.contains(' '), "{uri} carries a raw space");
        assert_eq!(decode(uri, "text/plain;charset=utf-8"), b"bytes-4");
    }

    #[test]
    fn media_type_keeps_several_parameters_and_drops_empty_ones() {
        let mut event = json!({ "links": { "1": {
            "@blob": 4, "contentType": " text/plain ;; charset=utf-8 ; format=flowed "
        } } });
        inline_blob_data_uris(&mut event, InlineShape::JsCalendarLink, fetch_ok).unwrap();
        let uri = event["links"]["1"]["href"].as_str().unwrap();
        assert_eq!(
            decode(uri, "text/plain;charset=utf-8;format=flowed"),
            b"bytes-4"
        );
    }

    #[test]
    fn media_type_characters_illegal_in_a_uri_are_percent_encoded() {
        let mut event = json!({ "links": { "1": {
            "@blob": 4, "contentType": "text/plain; name=\"rep ort,v2#1\u{e9}.txt\""
        } } });
        inline_blob_data_uris(&mut event, InlineShape::JsCalendarLink, fetch_ok).unwrap();
        let uri = event["links"]["1"]["href"].as_str().unwrap();
        assert_eq!(
            decode(uri, "text/plain;name=%22rep%20ort%2Cv2%231%C3%A9.txt%22"),
            b"bytes-4"
        );
    }

    #[test]
    fn a_percent_in_the_media_type_is_itself_escaped() {
        let mut card = json!({ "media": { "photo": {
            "@blob": 4, "mediaType": "image/png; note=100%"
        } } });
        inline_blob_data_uris(&mut card, InlineShape::JsContactResource, fetch_ok).unwrap();
        let uri = card["media"]["photo"]["uri"].as_str().unwrap();
        assert_eq!(decode(uri, "image/png;note=100%25"), b"bytes-4");
    }

    #[test]
    fn a_media_type_that_is_not_type_slash_subtype_defaults_to_octet_stream() {
        for bogus in ["png", "image/", "/png", "image/png/extra", ";charset=utf-8"] {
            let mut card = json!({ "media": { "photo": { "@blob": 4, "mediaType": bogus } } });
            inline_blob_data_uris(&mut card, InlineShape::JsContactResource, fetch_ok).unwrap();
            let uri = card["media"]["photo"]["uri"].as_str().unwrap();
            assert_eq!(decode(uri, DEFAULT_MEDIA_TYPE), b"bytes-4", "{bogus}");
        }
    }

    #[test]
    fn inlined_bytes_round_trip_exactly() {
        let payload: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let mut card = json!({ "media": { "photo": {
            "@blob": 1, "mediaType": "application/octet-stream; charset=binary"
        } } });
        inline_blob_data_uris(&mut card, InlineShape::JsContactResource, |_| {
            Ok(payload.clone())
        })
        .unwrap();
        let uri = card["media"]["photo"]["uri"].as_str().unwrap();
        assert_eq!(
            decode(uri, "application/octet-stream;charset=binary"),
            payload
        );
    }

    #[test]
    fn sentinels_nested_in_arrays_and_subobjects_are_all_inlined() {
        let mut value = json!({
            "locations": { "loc1": { "links": [
                { "@blob": 5, "contentType": "image/gif" },
                { "@blob": 6 }
            ] } },
            "links": { "top": { "@blob": 8, "contentType": "image/gif" } }
        });
        inline_blob_data_uris(&mut value, InlineShape::JsCalendarLink, fetch_ok).unwrap();
        let arr = value["locations"]["loc1"]["links"].as_array().unwrap();
        assert_eq!(
            decode(arr[0]["href"].as_str().unwrap(), "image/gif"),
            b"bytes-5"
        );
        assert_eq!(
            decode(arr[1]["href"].as_str().unwrap(), DEFAULT_MEDIA_TYPE),
            b"bytes-6"
        );
        assert_eq!(
            decode(value["links"]["top"]["href"].as_str().unwrap(), "image/gif"),
            b"bytes-8"
        );
        assert_no_blob_id(&value);
    }

    #[test]
    fn malformed_sentinel_is_rejected() {
        let mut value = json!({ "media": { "photo": { "@blob": "not-an-id" } } });
        let err = inline_blob_data_uris(&mut value, InlineShape::JsContactResource, fetch_ok)
            .expect_err("malformed sentinel");
        assert!(matches!(err, BlobWalkError::MalformedSentinel), "{err:?}");
    }

    #[test]
    fn resolver_failure_propagates() {
        let mut value = json!({ "media": { "photo": { "@blob": 9 } } });
        let err = inline_blob_data_uris(&mut value, InlineShape::JsContactResource, |_| {
            Err(BlobWalkError::resolver(JmapError::malformed("gone")))
        })
        .expect_err("resolver failure");
        assert!(matches!(err, BlobWalkError::Resolver(_)), "{err:?}");
    }

    #[test]
    fn the_resolver_error_kind_survives_the_walk() {
        let mut value = json!({ "media": { "photo": { "@blob": 9 } } });
        let err = inline_blob_data_uris(&mut value, InlineShape::JsContactResource, |_| {
            Err(BlobWalkError::resolver(JmapError::Sqlite(
                rusqlite::Error::QueryReturnedNoRows,
            )))
        })
        .expect_err("resolver failure")
        .into_source();
        assert!(matches!(err, JmapError::Sqlite(_)), "{err:?}");
        let mapped = crate::error::Error::from(err);
        assert!(mapped.aborts_run(), "{mapped} must abort the run");
        assert_eq!(mapped.exit_code(), 7);
    }

    #[test]
    fn a_malformed_sentinel_stays_a_blob_walk_error() {
        let err = BlobWalkError::MalformedSentinel.into_source();
        assert!(
            matches!(err, JmapError::Blob(BlobWalkError::MalformedSentinel)),
            "{err:?}"
        );
        assert_eq!(crate::error::Error::from(err).exit_code(), 5);
    }

    #[test]
    fn values_without_a_sentinel_are_untouched() {
        let original = json!({
            "@type": "Card",
            "media": { "photo": { "@type": "Media", "kind": "photo",
                "uri": "https://example.test/p.png", "mediaType": "image/png" } },
            "nickNames": { "n1": { "name": "Jay" } }
        });
        let mut value = original.clone();
        inline_blob_data_uris(&mut value, InlineShape::JsContactResource, |_| {
            panic!("fetch must not be called")
        })
        .unwrap();
        assert_eq!(value, original);
    }

    #[test]
    fn import_walker_still_produces_the_sentinel() {
        let mut value =
            json!({ "media": { "photo": { "blobId": "B1", "mediaType": "image/png" } } });
        import_blob_ids(&mut value, |id| {
            assert_eq!(id, "B1");
            Ok(11)
        })
        .unwrap();
        assert_eq!(value["media"]["photo"][SENTINEL_KEY], json!(11));
        assert!(value["media"]["photo"].get("blobId").is_none());
    }
}
