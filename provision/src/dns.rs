//! Captive-portal DNS responder for the setup access point (ADR 0004,
//! component 3).
//!
//! Every A question, whatever the name, is answered with the access point's
//! own address, so a phone that looks up `captive.apple.com` lands on the
//! setup page and shows it as a captive portal. AAAA questions get NOERROR
//! with no answer (so the phone does not wait for IPv6), and anything else -
//! another type, another class, another opcode - gets NOTIMP.
//!
//! What the caller does with sockets:
//!
//! - bind one UDP socket on the AP interface to `0.0.0.0:53` ([`PORT`]),
//! - for every datagram, call [`respond`] with the payload, the AP address
//!   and a buffer of at least [`MAX_REPLY`] bytes,
//! - on `Some(len)`, send `out[..len]` back to the sender; on `None`, send
//!   nothing.
//!
//! One question per query is all this answers; a query with more (or none)
//! is treated as malformed and dropped. Compression pointers do not occur in
//! queries, so a name containing one is malformed too. The answer itself
//! does use a pointer back to the question name.

/// UDP port to bind.
pub const PORT: u16 = 53;
/// TTL on the A record. Short: the address is only valid while the setup
/// access point is up.
pub const TTL: u32 = 60;
/// Longest reply [`respond`] can produce, for a query up to the 512 bytes
/// plain DNS allows.
pub const MAX_REPLY: usize = 512 + 16;

const HEADER: usize = 12;
const MAX_NAME: usize = 255;

const QR: u16 = 0x8000;
const AA: u16 = 0x0400;
const RD: u16 = 0x0100;
const OPCODE_MASK: u16 = 0x7800;

const RCODE_NOERROR: u16 = 0;
const RCODE_NOTIMP: u16 = 4;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

/// Answer one datagram received on port [`PORT`].
///
/// `ip` is the address to hand out (192.168.4.1 in setup mode). Returns the
/// number of bytes written to `out`, or `None` when nothing should be sent:
/// a malformed query, a response, or a buffer too small. Never panics.
pub fn respond(query: &[u8], ip: [u8; 4], out: &mut [u8]) -> Option<usize> {
    if query.len() < HEADER {
        return None;
    }
    let id = u16::from_be_bytes([query[0], query[1]]);
    let flags = u16::from_be_bytes([query[2], query[3]]);
    // Responses are not ours to answer.
    if flags & QR != 0 {
        return None;
    }
    let rd = flags & RD;
    // Not a standard query: nothing to parse, answer NOTIMP on the header.
    if flags & OPCODE_MASK != 0 {
        return write_header(out, id, flags, rd, RCODE_NOTIMP, 0, 0);
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        return None;
    }

    let qname_end = name_end(query, HEADER)?;
    let qtype = u16::from_be_bytes([*query.get(qname_end)?, *query.get(qname_end + 1)?]);
    let qclass = u16::from_be_bytes([*query.get(qname_end + 2)?, *query.get(qname_end + 3)?]);
    let question = query.get(HEADER..qname_end + 4)?;

    let (rcode, answers) = match (qclass, qtype) {
        (CLASS_IN, TYPE_A) => (RCODE_NOERROR, 1u16),
        (CLASS_IN, TYPE_AAAA) => (RCODE_NOERROR, 0),
        _ => (RCODE_NOTIMP, 0),
    };

    let len = HEADER + question.len() + if answers == 1 { 16 } else { 0 };
    if out.len() < len {
        return None;
    }
    write_header(out, id, flags, rd, rcode, 1, answers)?;
    out[HEADER..HEADER + question.len()].copy_from_slice(question);
    if answers == 1 {
        let mut i = HEADER + question.len();
        // Compression pointer back to the question name.
        out[i..i + 2].copy_from_slice(&[0xc0, HEADER as u8]);
        out[i + 2..i + 4].copy_from_slice(&TYPE_A.to_be_bytes());
        out[i + 4..i + 6].copy_from_slice(&CLASS_IN.to_be_bytes());
        out[i + 6..i + 10].copy_from_slice(&TTL.to_be_bytes());
        out[i + 10..i + 12].copy_from_slice(&4u16.to_be_bytes());
        out[i + 12..i + 16].copy_from_slice(&ip);
        i += 16;
        debug_assert_eq!(i, len);
    }
    Some(len)
}

/// Write the 12-byte reply header. Returns the length written.
fn write_header(
    out: &mut [u8],
    id: u16,
    query_flags: u16,
    rd: u16,
    rcode: u16,
    qdcount: u16,
    ancount: u16,
) -> Option<usize> {
    let out = out.get_mut(..HEADER)?;
    let flags = QR | AA | rd | (query_flags & OPCODE_MASK) | rcode;
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[4..6].copy_from_slice(&qdcount.to_be_bytes());
    out[6..8].copy_from_slice(&ancount.to_be_bytes());
    out[8..10].copy_from_slice(&0u16.to_be_bytes());
    out[10..12].copy_from_slice(&0u16.to_be_bytes());
    Some(HEADER)
}

/// Offset just past the zero byte that ends the name at `start`.
///
/// `None` for a truncated name, an over-long one, or one holding a
/// compression pointer (queries do not use compression; anything that does
/// is treated as malformed).
fn name_end(packet: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut total = 0usize;
    loop {
        let len = usize::from(*packet.get(i)?);
        if len == 0 {
            return Some(i + 1);
        }
        if len & 0xc0 != 0 {
            return None;
        }
        total += len + 1;
        if total > MAX_NAME {
            return None;
        }
        // The label itself has to be there.
        packet.get(i + 1..i + 1 + len)?;
        i += 1 + len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AP: [u8; 4] = [192, 168, 4, 1];

    /// A query for `name` (dotted) of type `qtype`, with RD set like a
    /// resolver sends it.
    fn query(name: &str, qtype: u16) -> std::vec::Vec<u8> {
        let mut p = std::vec::Vec::new();
        p.extend_from_slice(&0x1234u16.to_be_bytes());
        p.extend_from_slice(&RD.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes()); // qdcount
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            p.push(label.len() as u8);
            p.extend_from_slice(label.as_bytes());
        }
        p.push(0);
        p.extend_from_slice(&qtype.to_be_bytes());
        p.extend_from_slice(&CLASS_IN.to_be_bytes());
        p
    }

    fn flags(reply: &[u8]) -> u16 {
        u16::from_be_bytes([reply[2], reply[3]])
    }

    #[test]
    fn an_a_query_is_answered_with_the_access_point() {
        let q = query("captive.apple.com", TYPE_A);
        let mut out = [0u8; MAX_REPLY];
        let n = respond(&q, AP, &mut out).unwrap();
        let r = &out[..n];
        assert_eq!(&r[0..2], &0x1234u16.to_be_bytes());
        assert_eq!(flags(r), QR | AA | RD);
        assert_eq!(u16::from_be_bytes([r[4], r[5]]), 1); // qdcount
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1); // ancount
                                                         // The question is echoed verbatim.
        assert_eq!(&r[HEADER..q.len()], &q[HEADER..]);
        let a = &r[q.len()..];
        assert_eq!(&a[0..2], &[0xc0, 12]); // pointer to the question name
        assert_eq!(u16::from_be_bytes([a[2], a[3]]), TYPE_A);
        assert_eq!(u16::from_be_bytes([a[4], a[5]]), CLASS_IN);
        assert_eq!(u32::from_be_bytes([a[6], a[7], a[8], a[9]]), 60);
        assert_eq!(u16::from_be_bytes([a[10], a[11]]), 4);
        assert_eq!(&a[12..16], &AP);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn aaaa_gets_noerror_without_an_answer() {
        let q = query("captive.apple.com", TYPE_AAAA);
        let mut out = [0u8; MAX_REPLY];
        let n = respond(&q, AP, &mut out).unwrap();
        assert_eq!(flags(&out[..n]) & 0x000f, RCODE_NOERROR);
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 0);
        assert_eq!(n, HEADER + (q.len() - HEADER));
    }

    #[test]
    fn mx_gets_notimp() {
        let q = query("example.com", 15);
        let mut out = [0u8; MAX_REPLY];
        let n = respond(&q, AP, &mut out).unwrap();
        assert_eq!(flags(&out[..n]) & 0x000f, RCODE_NOTIMP);
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 0);
    }

    #[test]
    fn a_foreign_class_gets_notimp() {
        let mut q = query("example.com", TYPE_A);
        let n = q.len();
        q[n - 2..].copy_from_slice(&3u16.to_be_bytes()); // CHAOS
        let mut out = [0u8; MAX_REPLY];
        let n = respond(&q, AP, &mut out).unwrap();
        assert_eq!(flags(&out[..n]) & 0x000f, RCODE_NOTIMP);
    }

    #[test]
    fn an_inverse_query_gets_notimp_on_the_header_alone() {
        let mut q = query("example.com", TYPE_A);
        q[2..4].copy_from_slice(&(RD | 0x0800).to_be_bytes()); // opcode 1
        let mut out = [0u8; MAX_REPLY];
        let n = respond(&q, AP, &mut out).unwrap();
        assert_eq!(n, HEADER);
        assert_eq!(flags(&out[..n]) & 0x000f, RCODE_NOTIMP);
        assert_eq!(flags(&out[..n]) & OPCODE_MASK, 0x0800);
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 0);
    }

    #[test]
    fn the_root_name_is_a_valid_question() {
        let mut q = std::vec::Vec::new();
        q.extend_from_slice(&[0x11, 0x22, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        q.push(0); // root
        q.extend_from_slice(&TYPE_A.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        let mut out = [0u8; MAX_REPLY];
        let n = respond(&q, AP, &mut out).unwrap();
        assert_eq!(n, HEADER + 5 + 16);
    }

    #[test]
    fn responses_and_malformed_queries_are_dropped() {
        let mut out = [0u8; MAX_REPLY];
        // A response.
        let mut q = query("example.com", TYPE_A);
        q[2] |= 0x80;
        assert!(respond(&q, AP, &mut out).is_none());
        // Two questions.
        let mut q = query("example.com", TYPE_A);
        q[4..6].copy_from_slice(&2u16.to_be_bytes());
        assert!(respond(&q, AP, &mut out).is_none());
        // No question at all.
        let mut q = query("example.com", TYPE_A);
        q[4..6].copy_from_slice(&0u16.to_be_bytes());
        assert!(respond(&q, AP, &mut out).is_none());
        // A compression pointer in the question name.
        let mut q = query("example.com", TYPE_A);
        q[HEADER] = 0xc0;
        q[HEADER + 1] = 12;
        assert!(respond(&q, AP, &mut out).is_none());
        // A label length that runs off the end.
        let mut q = query("example.com", TYPE_A);
        q[HEADER] = 200;
        assert!(respond(&q, AP, &mut out).is_none());
        // Truncated before qtype/qclass.
        let q = query("example.com", TYPE_A);
        assert!(respond(&q[..q.len() - 3], AP, &mut out).is_none());
        // Header only.
        assert!(respond(&q[..HEADER], AP, &mut out).is_none());
        // Empty datagram.
        assert!(respond(&[], AP, &mut out).is_none());
        // Output buffer too small for the answer.
        let mut small = [0u8; 20];
        assert!(respond(&q, AP, &mut small).is_none());
    }

    #[test]
    fn a_name_over_255_bytes_is_malformed() {
        let label = "a".repeat(63);
        let name = std::format!("{label}.{label}.{label}.{label}.{label}");
        let q = query(&name, TYPE_A);
        let mut out = [0u8; MAX_REPLY];
        assert!(respond(&q, AP, &mut out).is_none());
    }

    #[test]
    fn every_prefix_and_mutation_is_survivable() {
        let q = query("captive.apple.com", TYPE_A);
        let mut out = [0u8; MAX_REPLY];
        for n in 0..=q.len() {
            let _ = respond(&q[..n], AP, &mut out);
        }
        for i in 0..q.len() {
            for b in [0u8, 1, 0xc0, 0x3f, 0xff] {
                let mut m = q.clone();
                m[i] = b;
                let _ = respond(&m, AP, &mut out);
            }
        }
    }
}
