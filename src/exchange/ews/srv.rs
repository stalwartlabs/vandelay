/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use std::cmp::Reverse;
use std::fs;
use std::net::UdpSocket;
use std::str;
use std::time::Duration;

const RESOLV_CONF: &str = "/etc/resolv.conf";
const DNS_PORT: u16 = 53;
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const TXN_ID: u16 = 0x1234;
const FLAGS_RECURSION_DESIRED: u16 = 0x0100;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;
const HEADER_LEN: usize = 12;
const QUESTION_TAIL: usize = 4;
const RR_FIXED_LEN: usize = 10;
const SRV_MIN_RDATA: usize = 7;
const UDP_PAYLOAD: usize = 1500;
const POINTER_TAG: u8 = 0xC0;
const POINTER_MASK: u8 = 0x3F;
const MAX_LABEL: usize = 63;
const MAX_NAME: usize = 255;
const MAX_POINTER_HOPS: usize = 8;
const MAX_RECORDS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvRecord {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    pub target: String,
}

#[derive(Debug, Clone, Copy)]
struct Message<'a>(&'a [u8]);

impl SrvRecord {
    pub fn lookup(qname: &str) -> Vec<SrvRecord> {
        let Some(query) = SrvRecord::query(qname) else {
            return Vec::new();
        };
        let mut out = SrvRecord::nameservers()
            .into_iter()
            .find_map(|ns| SrvRecord::ask(&ns, &query))
            .unwrap_or_default();
        SrvRecord::sort_by_preference(&mut out);
        out
    }

    fn sort_by_preference(records: &mut [SrvRecord]) {
        records.sort_by_key(|r| (r.priority, Reverse(r.weight)));
    }

    fn ask(nameserver: &str, query: &[u8]) -> Option<Vec<SrvRecord>> {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        let _ = socket.set_read_timeout(Some(DNS_TIMEOUT));
        socket.send_to(query, (nameserver, DNS_PORT)).ok()?;
        let mut buf = [0u8; UDP_PAYLOAD];
        let (n, _) = socket.recv_from(&mut buf).ok()?;
        Message(buf.get(..n)?).srv_records()
    }

    fn nameservers() -> Vec<String> {
        let Ok(text) = fs::read_to_string(RESOLV_CONF) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| {
                let line = line.split('#').next().unwrap_or_default().trim();
                let ns = line.strip_prefix("nameserver")?.trim();
                (!ns.is_empty()).then(|| ns.to_owned())
            })
            .collect()
    }

    fn query(qname: &str) -> Option<Vec<u8>> {
        let mut query = Vec::with_capacity(HEADER_LEN + qname.len() + 2 + QUESTION_TAIL);
        query.extend_from_slice(&TXN_ID.to_be_bytes());
        query.extend_from_slice(&FLAGS_RECURSION_DESIRED.to_be_bytes());
        query.extend_from_slice(&1u16.to_be_bytes());
        query.extend_from_slice(&[0u8; 6]);
        for label in qname.trim_end_matches('.').split('.') {
            if label.is_empty() {
                continue;
            }
            if label.len() > MAX_LABEL {
                return None;
            }
            query.push(u8::try_from(label.len()).ok()?);
            query.extend_from_slice(label.as_bytes());
        }
        query.push(0);
        query.extend_from_slice(&TYPE_SRV.to_be_bytes());
        query.extend_from_slice(&CLASS_IN.to_be_bytes());
        Some(query)
    }
}

impl<'a> Message<'a> {
    fn u16_at(self, at: usize) -> Option<u16> {
        let bytes = self.0.get(at..at.checked_add(2)?)?;
        let [hi, lo] = bytes else { return None };
        Some(u16::from_be_bytes([*hi, *lo]))
    }

    fn skip_name(self, mut cursor: usize) -> Option<usize> {
        loop {
            let len = *self.0.get(cursor)?;
            if len == 0 {
                return cursor.checked_add(1);
            }
            if len & POINTER_TAG == POINTER_TAG {
                return cursor.checked_add(2);
            }
            cursor = cursor.checked_add(1 + usize::from(len))?;
        }
    }

    fn read_name(self, mut cursor: usize) -> Option<String> {
        let mut out = String::new();
        let mut hops = 0;
        loop {
            let len = *self.0.get(cursor)?;
            if len == 0 {
                return Some(out);
            }
            if len & POINTER_TAG == POINTER_TAG {
                let low = *self.0.get(cursor.checked_add(1)?)?;
                hops += 1;
                if hops > MAX_POINTER_HOPS {
                    return None;
                }
                cursor = usize::from(len & POINTER_MASK) << 8 | usize::from(low);
                continue;
            }
            let start = cursor.checked_add(1)?;
            let end = start.checked_add(usize::from(len))?;
            let label = str::from_utf8(self.0.get(start..end)?).ok()?;
            if out.len() + label.len() + 1 > MAX_NAME {
                return None;
            }
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(label);
            cursor = end;
        }
    }

    fn srv_records(self) -> Option<Vec<SrvRecord>> {
        let questions = self.u16_at(4)?;
        let answers = self.u16_at(6)?;
        let mut cursor = HEADER_LEN;
        for _ in 0..questions {
            cursor = self.skip_name(cursor)?.checked_add(QUESTION_TAIL)?;
            if cursor > self.0.len() {
                return None;
            }
        }
        let mut out = Vec::new();
        for _ in 0..answers {
            cursor = self.skip_name(cursor)?;
            let rtype = self.u16_at(cursor)?;
            let rdlength = usize::from(self.u16_at(cursor.checked_add(8)?)?);
            cursor = cursor.checked_add(RR_FIXED_LEN)?;
            let rdata_end = cursor.checked_add(rdlength)?;
            if rdata_end > self.0.len() {
                return None;
            }
            if rtype == TYPE_SRV && rdlength >= SRV_MIN_RDATA && out.len() < MAX_RECORDS {
                out.push(SrvRecord {
                    priority: self.u16_at(cursor)?,
                    weight: self.u16_at(cursor + 2)?,
                    port: self.u16_at(cursor + 4)?,
                    target: self.read_name(cursor + 6)?,
                });
            }
            cursor = rdata_end;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(target_labels: &[&str], port: u16) -> Vec<u8> {
        let mut msg = SrvRecord::query("_autodiscover._tcp.example.com").unwrap();
        msg[6..8].copy_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&[0xC0, 12]);
        msg.extend_from_slice(&TYPE_SRV.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
        msg.extend_from_slice(&300u32.to_be_bytes());
        let mut rdata = Vec::new();
        rdata.extend_from_slice(&10u16.to_be_bytes());
        rdata.extend_from_slice(&5u16.to_be_bytes());
        rdata.extend_from_slice(&port.to_be_bytes());
        for label in target_labels {
            rdata.push(u8::try_from(label.len()).unwrap());
            rdata.extend_from_slice(label.as_bytes());
        }
        rdata.push(0);
        msg.extend_from_slice(&u16::try_from(rdata.len()).unwrap().to_be_bytes());
        msg.extend_from_slice(&rdata);
        msg
    }

    // Records with equal priority and weight keep their DNS answer order.
    #[test]
    fn equal_preference_records_keep_answer_order() {
        let record = |priority, weight, target: &str| SrvRecord {
            priority,
            weight,
            port: 443,
            target: target.to_owned(),
        };
        let mut records = vec![
            record(10, 5, "b.example.com"),
            record(10, 5, "a.example.com"),
            record(0, 1, "first.example.com"),
            record(10, 9, "heavy.example.com"),
            record(10, 5, "c.example.com"),
        ];
        SrvRecord::sort_by_preference(&mut records);
        let targets: Vec<&str> = records.iter().map(|r| r.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "first.example.com",
                "heavy.example.com",
                "b.example.com",
                "a.example.com",
                "c.example.com"
            ]
        );
    }

    #[test]
    fn parses_one_srv_answer() {
        let msg = answer(&["mail", "example", "com"], 8443);
        assert_eq!(
            Message(&msg).srv_records().unwrap(),
            vec![SrvRecord {
                priority: 10,
                weight: 5,
                port: 8443,
                target: "mail.example.com".to_owned(),
            }]
        );
    }

    #[test]
    fn truncated_answer_is_rejected() {
        let msg = answer(&["mail", "example", "com"], 443);
        for cut in HEADER_LEN..msg.len() {
            assert!(Message(&msg[..cut]).srv_records().is_none(), "cut {cut}");
        }
    }

    #[test]
    fn pointer_loop_is_bounded() {
        let mut msg = answer(&[], 443);
        let at = msg.len() - 1;
        msg.pop();
        msg.extend_from_slice(&[0xC0, u8::try_from(at).unwrap()]);
        msg[at - 8..at - 6].copy_from_slice(&8u16.to_be_bytes());
        assert!(Message(&msg).srv_records().is_none());
    }

    #[test]
    fn overlong_label_is_not_queried() {
        let label = "a".repeat(MAX_LABEL + 1);
        assert!(SrvRecord::query(&format!("{label}.example.com")).is_none());
        assert!(SrvRecord::query("_autodiscover._tcp.example.com").is_some());
    }
}
