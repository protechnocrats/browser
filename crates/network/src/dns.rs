//! The DNS wire format, as much of it as DNS over HTTPS needs (Milestone
//! 37, ADR 012): one question out (`A` or `AAAA`), the addresses back.
//!
//! Written by hand, like every format here, so it can be bounded: names of
//! at most 255 bytes, compression pointers followed at most
//! [`MAX_POINTER_HOPS`] times (and only backwards), at most
//! [`MAX_ANSWERS`] answer records read. The decoder never panics on any
//! input (fuzzed: `dns_message`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const MAX_POINTER_HOPS: usize = 16;
pub const MAX_ANSWERS: usize = 64;
const MAX_NAME_BYTES: usize = 255;

/// The record types asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    A,
    Aaaa,
}

impl QueryType {
    fn code(self) -> u16 {
        match self {
            QueryType::A => 1,
            QueryType::Aaaa => 28,
        }
    }
}

const TYPE_CNAME: u16 = 5;
const CLASS_IN: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsError(pub &'static str);

impl std::fmt::Display for DnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// The addresses an answer gave for the name asked about, and how long they
/// may be kept (the smallest TTL among them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub addresses: Vec<IpAddr>,
    pub ttl: u32,
}

/// A query for `host` (`id` 0, recursion desired — RFC 8484 §4.1 asks for
/// id 0 so responses are cacheable). `Err` for a name DNS cannot carry.
pub fn encode_query(host: &str, qtype: QueryType) -> Result<Vec<u8>, DnsError> {
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.len() > 253 {
        return Err(DnsError("name length"));
    }
    let mut out = Vec::with_capacity(18 + host.len());
    out.extend_from_slice(&[0, 0, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(DnsError("bad label"));
        }
        out.push(label.len() as u8);
        out.extend(label.bytes().map(|b| b.to_ascii_lowercase()));
    }
    out.push(0);
    out.extend_from_slice(&qtype.code().to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(out)
}

struct Reader<'a> {
    msg: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, DnsError> {
        let b = *self.msg.get(self.pos).ok_or(DnsError("truncated"))?;
        self.pos += 1;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, DnsError> {
        Ok(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }
    fn u32(&mut self) -> Result<u32, DnsError> {
        Ok(u32::from_be_bytes([self.u8()?, self.u8()?, self.u8()?, self.u8()?]))
    }
    fn take(&mut self, n: usize) -> Result<&[u8], DnsError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.msg.len()).ok_or(DnsError("truncated"))?;
        let bytes = &self.msg[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    /// A (possibly compressed) name, lowercased, dot-separated. Pointers
    /// must point backwards and are followed at most `MAX_POINTER_HOPS`
    /// times.
    fn name(&mut self) -> Result<String, DnsError> {
        let mut name = String::new();
        let mut pos = self.pos;
        let mut jumped = false;
        let mut hops = 0;
        loop {
            let len = *self.msg.get(pos).ok_or(DnsError("truncated name"))?;
            match len & 0xC0 {
                0x00 => {
                    pos += 1;
                    if len == 0 {
                        break;
                    }
                    let label = self.msg.get(pos..pos + len as usize).ok_or(DnsError("truncated label"))?;
                    if !name.is_empty() {
                        name.push('.');
                    }
                    name.extend(label.iter().map(|&b| (b as char).to_ascii_lowercase()));
                    if name.len() > MAX_NAME_BYTES {
                        return Err(DnsError("name too long"));
                    }
                    pos += len as usize;
                }
                0xC0 => {
                    let low = *self.msg.get(pos + 1).ok_or(DnsError("truncated pointer"))?;
                    let target = (usize::from(len & 0x3F) << 8) | usize::from(low);
                    hops += 1;
                    if hops > MAX_POINTER_HOPS || target >= pos {
                        return Err(DnsError("bad compression pointer"));
                    }
                    if !jumped {
                        self.pos = pos + 2;
                        jumped = true;
                    }
                    pos = target;
                }
                _ => return Err(DnsError("reserved label type")),
            }
        }
        if !jumped {
            self.pos = pos;
        }
        Ok(name)
    }
}

/// The `qtype` addresses for `host` in the response `msg`, following
/// `CNAME`s within the answer section. `Err` for a malformed message or an
/// error response; an answer with no addresses is `Ok` with none.
pub fn decode_answers(msg: &[u8], host: &str, qtype: QueryType) -> Result<Answer, DnsError> {
    let mut r = Reader { msg, pos: 0 };
    let _id = r.u16()?;
    let flags = r.u16()?;
    if flags & 0x8000 == 0 {
        return Err(DnsError("not a response"));
    }
    match flags & 0x000F {
        0 => {}
        3 => return Ok(Answer { addresses: Vec::new(), ttl: 0 }), // NXDOMAIN
        _ => return Err(DnsError("the resolver reported an error")),
    }
    let (qd, an) = (r.u16()?, r.u16()?);
    let _ = (r.u16()?, r.u16()?);
    for _ in 0..qd.min(4) {
        r.name()?;
        r.take(4)?;
    }
    if qd > 4 {
        return Err(DnsError("too many questions"));
    }
    let mut names = vec![host.trim_end_matches('.').to_ascii_lowercase()];
    let mut addresses = Vec::new();
    let mut ttl = u32::MAX;
    for _ in 0..usize::from(an).min(MAX_ANSWERS) {
        let owner = r.name()?;
        let (rtype, class, record_ttl, len) = (r.u16()?, r.u16()?, r.u32()?, r.u16()?);
        let rdata_start = r.pos;
        let rdata = r.take(usize::from(len))?;
        if class != CLASS_IN || !names.contains(&owner) {
            continue;
        }
        if rtype == TYPE_CNAME {
            let mut target = Reader { msg, pos: rdata_start };
            let alias = target.name()?;
            if names.len() < 8 && !names.contains(&alias) {
                names.push(alias);
            }
            continue;
        }
        let address = match (qtype, rtype, rdata.len()) {
            (QueryType::A, 1, 4) => IpAddr::V4(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])),
            (QueryType::Aaaa, 28, 16) => {
                let mut b = [0u8; 16];
                b.copy_from_slice(rdata);
                IpAddr::V6(Ipv6Addr::from(b))
            }
            _ => continue,
        };
        ttl = ttl.min(record_ttl);
        addresses.push(address);
    }
    Ok(Answer { ttl: if addresses.is_empty() { 0 } else { ttl }, addresses })
}

/// Base64url without padding (RFC 4648 §5) — the `?dns=` parameter.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        let emit = chunk.len() + 1;
        for i in 0..emit {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response to `query` with `records`: (owner offset 12 = the
    /// question's name via a pointer, type, rdata).
    pub(crate) fn response(query: &[u8], records: &[(u16, &[u8])], ttl: u32) -> Vec<u8> {
        let mut msg = query.to_vec();
        msg[2] = 0x81;
        msg[3] = 0x80;
        msg[7] = records.len() as u8;
        for (rtype, rdata) in records {
            msg.extend_from_slice(&[0xC0, 12]);
            msg.extend_from_slice(&rtype.to_be_bytes());
            msg.extend_from_slice(&CLASS_IN.to_be_bytes());
            msg.extend_from_slice(&ttl.to_be_bytes());
            msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            msg.extend_from_slice(rdata);
        }
        msg
    }

    #[test]
    fn a_query_round_trips_through_a_response() {
        let q = encode_query("Example.COM.", QueryType::A).unwrap();
        assert_eq!(&q[12..], b"\x07example\x03com\x00\x00\x01\x00\x01");
        let msg = response(&q, &[(1, &[93, 184, 216, 34]), (1, &[10, 0, 0, 1])], 300);
        let answer = decode_answers(&msg, "example.com", QueryType::A).unwrap();
        assert_eq!(answer.addresses, vec![IpAddr::from([93, 184, 216, 34]), IpAddr::from([10, 0, 0, 1])]);
        assert_eq!(answer.ttl, 300);
        let q6 = encode_query("example.com", QueryType::Aaaa).unwrap();
        let v6 = Ipv6Addr::LOCALHOST.octets();
        let msg = response(&q6, &[(28, &v6), (1, &[1, 2, 3, 4])], 60);
        assert_eq!(decode_answers(&msg, "example.com", QueryType::Aaaa).unwrap().addresses, vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]);
    }

    #[test]
    fn cnames_are_followed_within_the_answer() {
        let q = encode_query("www.example.com", QueryType::A).unwrap();
        let mut msg = response(&q, &[], 0);
        msg[7] = 2;
        // www.example.com CNAME cdn.example.net
        msg.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60]);
        let alias = b"\x03cdn\x07example\x03net\x00";
        msg.extend_from_slice(&(alias.len() as u16).to_be_bytes());
        let alias_at = msg.len();
        msg.extend_from_slice(alias);
        // cdn.example.net A 1.2.3.4 (owner by pointer to the alias)
        msg.extend_from_slice(&[0xC0, alias_at as u8, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 1, 2, 3, 4]);
        let answer = decode_answers(&msg, "www.example.com", QueryType::A).unwrap();
        assert_eq!(answer.addresses, vec![IpAddr::from([1, 2, 3, 4])]);
        assert_eq!(answer.ttl, 30);
    }

    #[test]
    fn records_for_other_names_are_ignored() {
        let q = encode_query("example.com", QueryType::A).unwrap();
        let mut msg = response(&q, &[], 0);
        msg[7] = 1;
        msg.extend_from_slice(b"\x05other\x03com\x00\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x06\x06\x06\x06");
        assert!(decode_answers(&msg, "example.com", QueryType::A).unwrap().addresses.is_empty());
    }

    #[test]
    fn hostile_messages_are_errors_not_panics() {
        let q = encode_query("example.com", QueryType::A).unwrap();
        // A pointer loop, a forward pointer, a reserved label type.
        for bad_name in [&[0xC0u8, 12][..], &[0xC0, 0xFF], &[0x80, 0]] {
            let mut msg = response(&q, &[], 0);
            msg[7] = 1;
            msg.truncate(12);
            msg[5] = 0;
            msg.extend_from_slice(bad_name);
            assert!(decode_answers(&msg, "example.com", QueryType::A).is_err());
        }
        // Truncated anywhere.
        let full = response(&q, &[(1, &[1, 2, 3, 4])], 5);
        for n in 0..full.len() {
            let _ = decode_answers(&full[..n], "example.com", QueryType::A);
        }
        // A query (not a response), and a server failure.
        assert!(decode_answers(&q, "example.com", QueryType::A).is_err());
        let mut fail = full.clone();
        fail[3] = 0x82;
        assert!(decode_answers(&fail, "example.com", QueryType::A).is_err());
        // NXDOMAIN is an empty answer.
        let mut nx = full;
        nx[3] = 0x83;
        assert!(decode_answers(&nx, "example.com", QueryType::A).unwrap().addresses.is_empty());
        // Names DNS cannot carry.
        for host in ["", "a..b", &"a".repeat(64), &format!("{}.com", "a.".repeat(130)), "spa ce.com"] {
            assert!(encode_query(host, QueryType::A).is_err(), "{host}");
        }
    }

    #[test]
    fn base64url_matches_rfc_4648() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }
}
