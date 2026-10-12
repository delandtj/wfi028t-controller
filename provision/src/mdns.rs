//! mDNS responder for `<hostname>.local` on the station side (ADR 0004,
//! component 3).
//!
//! One record, one name: an A record for our own host name, so
//! `http://wfi028t-XXXX.local/` works on the home network without a DNS
//! entry. No service records (no `_http._tcp`), no probing, no conflict
//! resolution, and no known-answer suppression: a query that asks for our
//! name gets an answer even if it already lists one.
//!
//! What the caller does with sockets:
//!
//! - on the station interface, join the multicast group 224.0.0.251 and bind
//!   a UDP socket to `0.0.0.0:5353` ([`PORT`]),
//! - for every datagram, call [`respond`] with the payload, the host name
//!   (the `<hostname>` part, without `.local`), our IPv4 address, the source
//!   port of the datagram, and a buffer of at least [`MAX_REPLY`] bytes,
//! - on `Some(r)`, send `out[..r.len]` to the sender if `r.unicast`, else to
//!   224.0.0.251:5353; on `None`, send nothing.
//!
//! A reply is unicast when the question had the QU bit (0x8000 in qclass) or
//! the query came from a port other than 5353 (a legacy one-shot resolver).
//!
//! Name matching is case-insensitive, as mDNS requires. Compression pointers
//! are not expected in a query's question section; a question that holds one
//! ends the parse, and only the questions read before it count.

/// UDP port to bind, and the port of the multicast group.
pub const PORT: u16 = 5353;
/// Multicast address a non-unicast reply goes to.
pub const MULTICAST_IPV4: [u8; 4] = [224, 0, 0, 251];
/// TTL on the A record, in seconds (the usual value for host records).
pub const TTL: u32 = 120;
/// Longest host name [`respond`] accepts (one DNS label).
pub const MAX_HOSTNAME: usize = 63;
/// Longest reply [`respond`] can produce.
pub const MAX_REPLY: usize = HEADER + 1 + MAX_HOSTNAME + 7 + RECORD;

/// Bytes of an A record after its name: type, class, TTL, length, address.
const RECORD: usize = 14;

const HEADER: usize = 12;
const QR: u16 = 0x8000;
const AA: u16 = 0x0400;
const OPCODE_MASK: u16 = 0x7800;
/// Unicast-response bit in qclass.
const QU: u16 = 0x8000;
/// Cache-flush bit in an answer's class.
const CACHE_FLUSH: u16 = 0x8000;
const CLASS_MASK: u16 = 0x7fff;

const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const CLASS_ANY: u16 = 255;

const LOCAL: &[u8] = b"local";

/// A response to send: `out[..len]`, unicast to the sender or multicast.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Response {
    /// Bytes written at the start of the caller's buffer.
    pub len: usize,
    /// True: send to the datagram's source address. False: send to
    /// [`MULTICAST_IPV4`]:[`PORT`].
    pub unicast: bool,
}

/// Answer one datagram received on port [`PORT`].
///
/// Returns `None` when nothing should be sent: a response rather than a
/// query, no question for our name, a malformed packet, an unusable host
/// name, or a buffer smaller than the reply. Never panics.
pub fn respond(
    packet: &[u8],
    hostname: &str,
    ip: [u8; 4],
    src_port: u16,
    out: &mut [u8],
) -> Option<Response> {
    let host = hostname.as_bytes();
    if host.is_empty() || host.len() > MAX_HOSTNAME {
        return None;
    }
    if packet.len() < HEADER {
        return None;
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    // Only queries, and only standard ones.
    if flags & QR != 0 || flags & OPCODE_MASK != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount == 0 {
        return None;
    }

    let mut matched = false;
    let mut unicast = src_port != PORT;
    let mut at = HEADER;
    for _ in 0..qdcount {
        // A malformed question ends the parse; what matched before it stands.
        let Some((end, is_ours)) = read_name(packet, at, host) else {
            break;
        };
        let Some(qtype) = be16(packet, end) else {
            break;
        };
        let Some(qclass) = be16(packet, end + 2) else {
            break;
        };
        at = end + 4;
        let wants_a = qtype == TYPE_A || qtype == TYPE_ANY;
        let class = qclass & CLASS_MASK;
        let class_ok = class == CLASS_IN || class == CLASS_ANY;
        if is_ours && wants_a && class_ok {
            matched = true;
            if qclass & QU != 0 {
                unicast = true;
            }
        }
    }
    if !matched {
        return None;
    }

    // Answer only, no question echo, so no compression pointer is possible:
    // the name is written out in full.
    let len = HEADER + 1 + host.len() + 7 + RECORD;
    let out = out.get_mut(..len)?;
    // ID 0: an mDNS response stands on its own, it does not match a query.
    out[0..2].copy_from_slice(&0u16.to_be_bytes());
    out[2..4].copy_from_slice(&(QR | AA).to_be_bytes());
    out[4..6].copy_from_slice(&0u16.to_be_bytes()); // qdcount
    out[6..8].copy_from_slice(&1u16.to_be_bytes()); // ancount
    out[8..10].copy_from_slice(&0u16.to_be_bytes());
    out[10..12].copy_from_slice(&0u16.to_be_bytes());
    let mut i = HEADER;
    out[i] = host.len() as u8;
    out[i + 1..i + 1 + host.len()].copy_from_slice(host);
    i += 1 + host.len();
    out[i] = LOCAL.len() as u8;
    out[i + 1..i + 1 + LOCAL.len()].copy_from_slice(LOCAL);
    i += 1 + LOCAL.len();
    out[i] = 0; // root
    i += 1;
    out[i..i + 2].copy_from_slice(&TYPE_A.to_be_bytes());
    out[i + 2..i + 4].copy_from_slice(&(CLASS_IN | CACHE_FLUSH).to_be_bytes());
    out[i + 4..i + 8].copy_from_slice(&TTL.to_be_bytes());
    out[i + 8..i + 10].copy_from_slice(&4u16.to_be_bytes());
    out[i + 10..i + 14].copy_from_slice(&ip);
    debug_assert_eq!(i + RECORD, len);
    Some(Response { len, unicast })
}

fn be16(packet: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]))
}

/// Walk the name at `start`, telling whether it is `<host>.local`.
///
/// Returns the offset just past the terminating zero byte. `None` for a
/// truncated name or one holding a compression pointer.
fn read_name(packet: &[u8], start: usize, host: &[u8]) -> Option<(usize, bool)> {
    let mut i = start;
    let mut label = 0usize;
    let mut ours = true;
    loop {
        let len = usize::from(*packet.get(i)?);
        if len == 0 {
            // `<host>.local` has exactly two labels.
            return Some((i + 1, ours && label == 2));
        }
        if len & 0xc0 != 0 {
            return None;
        }
        let bytes = packet.get(i + 1..i + 1 + len)?;
        match label {
            0 => ours &= eq_ignore_case(bytes, host),
            1 => ours &= eq_ignore_case(bytes, LOCAL),
            _ => ours = false,
        }
        label += 1;
        i += 1 + len;
    }
}

fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "wfi028t-1a2b";
    const IP: [u8; 4] = [192, 168, 1, 42];

    /// One query packet with the given questions: (name, qtype, qclass).
    fn query(questions: &[(&str, u16, u16)]) -> std::vec::Vec<u8> {
        let mut p = std::vec::Vec::new();
        p.extend_from_slice(&0u16.to_be_bytes()); // ID 0, as mDNS queries use
        p.extend_from_slice(&0u16.to_be_bytes()); // flags: query, opcode 0
        p.extend_from_slice(&(questions.len() as u16).to_be_bytes());
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for (name, qtype, qclass) in questions {
            for label in name.split('.') {
                p.push(label.len() as u8);
                p.extend_from_slice(label.as_bytes());
            }
            p.push(0);
            p.extend_from_slice(&qtype.to_be_bytes());
            p.extend_from_slice(&qclass.to_be_bytes());
        }
        p
    }

    fn ours(qtype: u16, qclass: u16) -> std::vec::Vec<u8> {
        query(&[("wfi028t-1a2b.local", qtype, qclass)])
    }

    #[test]
    fn an_a_query_for_our_name_is_answered() {
        let q = ours(TYPE_A, CLASS_IN);
        let mut out = [0u8; MAX_REPLY];
        let r = respond(&q, HOST, IP, PORT, &mut out).unwrap();
        assert!(!r.unicast);
        let b = &out[..r.len];
        assert_eq!(&b[0..2], &[0, 0]); // ID 0
        assert_eq!(u16::from_be_bytes([b[2], b[3]]), QR | AA);
        assert_eq!(u16::from_be_bytes([b[4], b[5]]), 0); // no question echo
        assert_eq!(u16::from_be_bytes([b[6], b[7]]), 1); // one answer
        let mut name = std::vec::Vec::new();
        name.push(HOST.len() as u8);
        name.extend_from_slice(HOST.as_bytes());
        name.extend_from_slice(b"\x05local\x00");
        assert_eq!(&b[HEADER..HEADER + name.len()], &name[..]);
        let rr = &b[HEADER + name.len()..];
        assert_eq!(u16::from_be_bytes([rr[0], rr[1]]), TYPE_A);
        assert_eq!(u16::from_be_bytes([rr[2], rr[3]]), 0x8001); // IN + flush
        assert_eq!(u32::from_be_bytes([rr[4], rr[5], rr[6], rr[7]]), 120);
        assert_eq!(u16::from_be_bytes([rr[8], rr[9]]), 4);
        assert_eq!(&rr[10..14], &IP);
        assert_eq!(rr.len(), 14);
    }

    #[test]
    fn the_name_match_ignores_case() {
        let q = query(&[("WFI028T-1A2B.Local", TYPE_A, CLASS_IN)]);
        let mut out = [0u8; MAX_REPLY];
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_some());
    }

    #[test]
    fn any_and_class_any_also_match() {
        let mut out = [0u8; MAX_REPLY];
        assert!(respond(&ours(TYPE_ANY, CLASS_IN), HOST, IP, PORT, &mut out).is_some());
        assert!(respond(&ours(TYPE_A, CLASS_ANY), HOST, IP, PORT, &mut out).is_some());
    }

    #[test]
    fn the_qu_bit_asks_for_a_unicast_reply() {
        let q = ours(TYPE_A, CLASS_IN | QU);
        let mut out = [0u8; MAX_REPLY];
        let r = respond(&q, HOST, IP, PORT, &mut out).unwrap();
        assert!(r.unicast);
    }

    #[test]
    fn a_legacy_source_port_asks_for_a_unicast_reply() {
        let q = ours(TYPE_A, CLASS_IN);
        let mut out = [0u8; MAX_REPLY];
        let r = respond(&q, HOST, IP, 51234, &mut out).unwrap();
        assert!(r.unicast);
    }

    #[test]
    fn our_name_among_several_questions_is_found() {
        let q = query(&[
            ("_services._dns-sd._udp.local", 12, CLASS_IN),
            ("other-box.local", TYPE_A, CLASS_IN),
            ("wfi028t-1a2b.local", TYPE_A, CLASS_IN | QU),
        ]);
        let mut out = [0u8; MAX_REPLY];
        let r = respond(&q, HOST, IP, PORT, &mut out).unwrap();
        assert!(r.unicast);
    }

    #[test]
    fn other_names_types_and_responses_are_ignored() {
        let mut out = [0u8; MAX_REPLY];
        // Another host.
        let q = query(&[("other-box.local", TYPE_A, CLASS_IN)]);
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_none());
        // Right label, wrong domain.
        let q = query(&[("wfi028t-1a2b.lan", TYPE_A, CLASS_IN)]);
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_none());
        // A sub-name of ours.
        let q = query(&[("x.wfi028t-1a2b.local", TYPE_A, CLASS_IN)]);
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_none());
        // A near miss in the host label.
        let q = query(&[("wfi028t-1a2c.local", TYPE_A, CLASS_IN)]);
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_none());
        // Our name, but AAAA and SRV.
        assert!(respond(&ours(28, CLASS_IN), HOST, IP, PORT, &mut out).is_none());
        assert!(respond(&ours(33, CLASS_IN), HOST, IP, PORT, &mut out).is_none());
        // Our name, foreign class.
        assert!(respond(&ours(TYPE_A, 3), HOST, IP, PORT, &mut out).is_none());
        // A response, not a query.
        let mut q = ours(TYPE_A, CLASS_IN);
        q[2] |= 0x80;
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_none());
        // An update (opcode 5).
        let mut q = ours(TYPE_A, CLASS_IN);
        q[2..4].copy_from_slice(&0x2800u16.to_be_bytes());
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_none());
        // No questions.
        let mut q = ours(TYPE_A, CLASS_IN);
        q[4..6].copy_from_slice(&0u16.to_be_bytes());
        assert!(respond(&q, HOST, IP, PORT, &mut out).is_none());
    }

    #[test]
    fn malformed_packets_and_bad_arguments_are_dropped() {
        let mut out = [0u8; MAX_REPLY];
        let q = ours(TYPE_A, CLASS_IN);
        // Truncated after the name.
        assert!(respond(&q[..q.len() - 2], HOST, IP, PORT, &mut out).is_none());
        // Header only, empty.
        assert!(respond(&q[..HEADER], HOST, IP, PORT, &mut out).is_none());
        assert!(respond(&[], HOST, IP, PORT, &mut out).is_none());
        // A compression pointer where a label belongs.
        let mut m = q.clone();
        m[HEADER] = 0xc0;
        m[HEADER + 1] = 12;
        assert!(respond(&m, HOST, IP, PORT, &mut out).is_none());
        // A label running off the end.
        let mut m = q.clone();
        m[HEADER] = 200;
        assert!(respond(&m, HOST, IP, PORT, &mut out).is_none());
        // qdcount larger than the questions present: what parsed still wins.
        let mut m = q.clone();
        m[4..6].copy_from_slice(&3u16.to_be_bytes());
        assert!(respond(&m, HOST, IP, PORT, &mut out).is_some());
        // An empty or over-long host name is not answerable.
        assert!(respond(&q, "", IP, PORT, &mut out).is_none());
        let long = "a".repeat(64);
        assert!(respond(&q, &long, IP, PORT, &mut out).is_none());
        // Output buffer too small.
        let mut small = [0u8; 32];
        assert!(respond(&q, HOST, IP, PORT, &mut small).is_none());
    }

    #[test]
    fn every_prefix_and_mutation_is_survivable() {
        let q = query(&[
            ("wfi028t-1a2b.local", TYPE_A, CLASS_IN | QU),
            ("other-box.local", TYPE_ANY, CLASS_IN),
        ]);
        let mut out = [0u8; MAX_REPLY];
        for n in 0..=q.len() {
            let _ = respond(&q[..n], HOST, IP, PORT, &mut out);
        }
        for i in 0..q.len() {
            for b in [0u8, 1, 0xc0, 0x3f, 0xff] {
                let mut m = q.clone();
                m[i] = b;
                let _ = respond(&m, HOST, IP, PORT, &mut out);
            }
        }
    }
}
