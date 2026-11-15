/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::fmt::Write;

use crate::net::xml::XmlEscape;

pub const NS_DAV: &str = "DAV:";
pub const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const NS_CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const NS_APPLE_ICAL: &str = "http://apple.com/ns/ical/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Propfind {
    CurrentUserPrincipal,
    CalendarHomeSet,
    AddressbookHomeSet,
    PrincipalAndCalendarHomeSet,
    PrincipalAndAddressbookHomeSet,
    CalendarCollections,
    AddressbookCollections,
    WebdavListing,
    DavItems,
}

impl Propfind {
    pub fn body(self) -> &'static str {
        match self {
            Propfind::CurrentUserPrincipal => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind xmlns:d=\"DAV:\">\
                 <d:prop><d:current-user-principal/></d:prop>\
                 </d:propfind>"
            }
            Propfind::CalendarHomeSet => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
                 <d:prop><c:calendar-home-set/></d:prop>\
                 </d:propfind>"
            }
            Propfind::AddressbookHomeSet => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
                 <d:prop><c:addressbook-home-set/></d:prop>\
                 </d:propfind>"
            }
            Propfind::PrincipalAndCalendarHomeSet => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
                 <d:prop>\
                 <d:current-user-principal/>\
                 <c:calendar-home-set/>\
                 </d:prop>\
                 </d:propfind>"
            }
            Propfind::PrincipalAndAddressbookHomeSet => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
                 <d:prop>\
                 <d:current-user-principal/>\
                 <c:addressbook-home-set/>\
                 </d:prop>\
                 </d:propfind>"
            }
            Propfind::CalendarCollections => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind \
                   xmlns:d=\"DAV:\" \
                   xmlns:c=\"urn:ietf:params:xml:ns:caldav\" \
                   xmlns:ic=\"http://apple.com/ns/ical/\">\
                 <d:prop>\
                 <d:resourcetype/>\
                 <d:displayname/>\
                 <c:calendar-description/>\
                 <c:supported-calendar-component-set/>\
                 <c:calendar-timezone/>\
                 <ic:calendar-color/>\
                 <ic:calendar-order/>\
                 </d:prop>\
                 </d:propfind>"
            }
            Propfind::AddressbookCollections => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind \
                   xmlns:d=\"DAV:\" \
                   xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
                 <d:prop>\
                 <d:resourcetype/>\
                 <d:displayname/>\
                 <c:addressbook-description/>\
                 </d:prop>\
                 </d:propfind>"
            }
            Propfind::WebdavListing => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind xmlns:d=\"DAV:\">\
                 <d:prop>\
                 <d:resourcetype/>\
                 <d:displayname/>\
                 <d:getetag/>\
                 <d:getcontenttype/>\
                 <d:getlastmodified/>\
                 <d:getcontentlength/>\
                 <d:creationdate/>\
                 </d:prop>\
                 </d:propfind>"
            }
            Propfind::DavItems => {
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <d:propfind xmlns:d=\"DAV:\">\
                 <d:prop>\
                 <d:resourcetype/>\
                 <d:getetag/>\
                 <d:getcontenttype/>\
                 <d:getlastmodified/>\
                 </d:prop>\
                 </d:propfind>"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Multiget {
    Calendar,
    Addressbook,
}

impl Multiget {
    pub fn body<'h, I>(self, hrefs: I) -> String
    where
        I: IntoIterator<Item = &'h str>,
        I::IntoIter: ExactSizeIterator,
    {
        let hrefs = hrefs.into_iter();
        let (head, tail) = match self {
            Multiget::Calendar => (
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <c:calendar-multiget \
                   xmlns:d=\"DAV:\" \
                   xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
                 <d:prop>\
                 <d:getetag/>\
                 <c:calendar-data/>\
                 </d:prop>",
                "</c:calendar-multiget>",
            ),
            Multiget::Addressbook => (
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <c:addressbook-multiget \
                   xmlns:d=\"DAV:\" \
                   xmlns:c=\"urn:ietf:params:xml:ns:carddav\">\
                 <d:prop>\
                 <d:getetag/>\
                 <c:address-data/>\
                 </d:prop>",
                "</c:addressbook-multiget>",
            ),
        };
        let mut out = String::with_capacity(256 + hrefs.len() * 64);
        out.push_str(head);
        for h in hrefs {
            let _ = write!(out, "<d:href>{}</d:href>", XmlEscape(h));
        }
        out.push_str(tail);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dav::href::Href;

    #[test]
    fn principal_body_matches_expected_shape() {
        let body = Propfind::CurrentUserPrincipal.body();
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<d:current-user-principal/>"));
        assert!(body.contains("xmlns:d=\"DAV:\""));
    }

    #[test]
    fn calendar_home_set_includes_caldav_namespace() {
        let body = Propfind::CalendarHomeSet.body();
        assert!(body.contains("xmlns:c=\"urn:ietf:params:xml:ns:caldav\""));
        assert!(body.contains("<c:calendar-home-set/>"));
    }

    #[test]
    fn addressbook_home_set_includes_carddav_namespace() {
        let body = Propfind::AddressbookHomeSet.body();
        assert!(body.contains("xmlns:c=\"urn:ietf:params:xml:ns:carddav\""));
        assert!(body.contains("<c:addressbook-home-set/>"));
    }

    #[test]
    fn calendar_multiget_lists_hrefs_in_order() {
        let hrefs = [
            Href::from_normalised("/dav/cal/u/d/a.ics".to_owned()),
            Href::from_normalised("/dav/cal/u/d/b.ics".to_owned()),
        ];
        let body = Multiget::Calendar.body(hrefs.iter().map(Href::as_str));
        let a_pos = body.find("/a.ics").unwrap();
        let b_pos = body.find("/b.ics").unwrap();
        assert!(a_pos < b_pos);
        assert!(body.contains("<c:calendar-data/>"));
    }

    #[test]
    fn addressbook_multiget_emits_address_data_element() {
        let hrefs = [Href::from_normalised("/dav/card/u/d/a.vcf".to_owned())];
        let body = Multiget::Addressbook.body(hrefs.iter().map(Href::as_str));
        assert!(body.contains("<c:address-data/>"));
        assert!(body.contains("<d:href>/dav/card/u/d/a.vcf</d:href>"));
    }

    #[test]
    fn escape_handles_xml_metacharacters() {
        let h = Href::from_normalised("/dav/cal/u/d/&lt;weird&gt;.ics".to_owned());
        let body = Multiget::Calendar.body([h.as_str()]);
        assert!(body.contains("&amp;lt;weird&amp;gt;.ics"));
    }

    #[test]
    fn webdav_listing_requests_filesystem_props() {
        let body = Propfind::WebdavListing.body();
        assert!(body.contains("<d:getcontenttype/>"));
        assert!(body.contains("<d:getlastmodified/>"));
        assert!(body.contains("<d:getcontentlength/>"));
        assert!(body.contains("<d:creationdate/>"));
    }
}
