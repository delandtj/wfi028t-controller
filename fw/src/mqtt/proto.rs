//! MQTT 3.1.1 packet encoding and decoding. No I/O, no allocation, no state.
//!
//! Only what the controller needs: CONNECT/CONNACK, PUBLISH at QoS 0 (with
//! retain), SUBSCRIBE/SUBACK, PINGREQ/PINGRESP and DISCONNECT. Everything
//! here is a pure function over byte slices, so it is testable on the host
//! (see the module comment in [`crate::mqtt::json`] for how).
//!
//! Why hand-rolled instead of a crate: see [`crate::mqtt`].
//!
//! Byte order is MQTT's: lengths are big-endian u16, and the remaining length
//! is the protocol's 7-bit variable byte integer.

/// Protocol level of MQTT 3.1.1.
pub const PROTOCOL_LEVEL: u8 = 4;

/// A complete PINGREQ packet.
pub const PINGREQ: [u8; 2] = [0xc0, 0x00];

/// A complete DISCONNECT packet.
pub const DISCONNECT: [u8; 2] = [0xe0, 0x00];

/// Largest encoded variable byte integer.
pub const MAX_VARINT_LEN: usize = 4;

/// Bytes a PUBLISH header can take: type, up to 4 length bytes, topic length.
pub const MAX_PUBLISH_HEADER: usize = 1 + MAX_VARINT_LEN + 2;

/// What went wrong encoding or decoding a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The output buffer is too small for the packet.
    BufferTooSmall,
    /// A string was longer than the protocol's 65535 bytes, or a packet
    /// exceeded the 4-byte remaining length.
    TooLong,
    /// The bytes are not a well-formed packet of the expected shape.
    Malformed,
    /// A topic name was not valid UTF-8.
    NotUtf8,
    /// The broker refused the connection with this CONNACK return code
    /// (1 = bad protocol version, 4 = bad credentials, 5 = not authorised).
    ConnectionRefused(u8),
}

/// Packet types the client reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    /// Answer to our CONNECT.
    Connack,
    /// A message on a topic we subscribed to.
    Publish,
    /// Answer to our SUBSCRIBE.
    Suback,
    /// Answer to our PINGREQ.
    Pingresp,
    /// Anything else, by its 4-bit type code.
    Other(u8),
}

impl PacketType {
    /// Classify the high nibble of a fixed header.
    #[must_use]
    pub const fn from_header(header: u8) -> Self {
        match header >> 4 {
            2 => Self::Connack,
            3 => Self::Publish,
            9 => Self::Suback,
            13 => Self::Pingresp,
            other => Self::Other(other),
        }
    }
}

/// The last will the broker publishes when we vanish.
#[derive(Debug, Clone, Copy)]
pub struct Will<'a> {
    /// Topic to publish on.
    pub topic: &'a str,
    /// Payload to publish.
    pub payload: &'a str,
    /// Whether the broker retains it.
    pub retain: bool,
}

/// Everything a CONNECT carries.
#[derive(Debug, Clone, Copy)]
pub struct Connect<'a> {
    /// Client identifier; the broker keys the session on it.
    pub client_id: &'a str,
    /// Keepalive in seconds, as promised to the broker.
    pub keepalive_s: u16,
    /// User name, if the broker wants one.
    pub user: Option<&'a str>,
    /// Password; ignored unless `user` is set, as the protocol requires.
    pub pass: Option<&'a str>,
    /// Last will and testament.
    pub will: Option<Will<'a>>,
}

/// Encode the variable byte integer `value` at the start of `out`.
///
/// # Errors
///
/// [`Error::TooLong`] above the protocol maximum of 268 435 455,
/// [`Error::BufferTooSmall`] if `out` cannot hold the digits.
pub fn encode_varint(mut value: u32, out: &mut [u8]) -> Result<usize, Error> {
    if value > 268_435_455 {
        return Err(Error::TooLong);
    }
    let mut len = 0;
    loop {
        let mut byte = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            byte |= 0x80;
        }
        *out.get_mut(len).ok_or(Error::BufferTooSmall)? = byte;
        len += 1;
        if value == 0 {
            return Ok(len);
        }
    }
}

/// Decode a variable byte integer, returning the value and the bytes it used.
///
/// # Errors
///
/// [`Error::Malformed`] if the digits never terminate within four bytes.
/// `Ok(None)` means the slice is a valid prefix but not yet complete.
pub fn decode_varint(bytes: &[u8]) -> Result<Option<(u32, usize)>, Error> {
    let mut value = 0u32;
    let mut multiplier = 1u32;
    for (i, &byte) in bytes.iter().take(MAX_VARINT_LEN).enumerate() {
        value += u32::from(byte & 0x7f) * multiplier;
        if byte & 0x80 == 0 {
            return Ok(Some((value, i + 1)));
        }
        multiplier *= 128;
    }
    if bytes.len() >= MAX_VARINT_LEN {
        return Err(Error::Malformed);
    }
    Ok(None)
}

/// Write a length-prefixed MQTT string at `out[at..]`, returning the new
/// offset.
fn put_str(out: &mut [u8], at: usize, text: &str) -> Result<usize, Error> {
    let bytes = text.as_bytes();
    let len = u16::try_from(bytes.len()).map_err(|_| Error::TooLong)?;
    let end = at + 2 + bytes.len();
    if end > out.len() {
        return Err(Error::BufferTooSmall);
    }
    out[at..at + 2].copy_from_slice(&len.to_be_bytes());
    out[at + 2..end].copy_from_slice(bytes);
    Ok(end)
}

/// Read a length-prefixed MQTT string at `bytes[at..]`.
fn take_str(bytes: &[u8], at: usize) -> Result<(&str, usize), Error> {
    let len_bytes = bytes.get(at..at + 2).ok_or(Error::Malformed)?;
    let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    let text = bytes.get(at + 2..at + 2 + len).ok_or(Error::Malformed)?;
    Ok((
        core::str::from_utf8(text).map_err(|_| Error::NotUtf8)?,
        at + 2 + len,
    ))
}

/// Encode a CONNECT packet into `out`.
///
/// Always a clean session: the controller keeps no session state worth
/// resuming, and a clean session is what makes a reconnect idempotent (the
/// subscriptions and the retained state are published again anyway).
///
/// # Errors
///
/// [`Error::BufferTooSmall`] if the packet does not fit, [`Error::TooLong`]
/// for a field over 65535 bytes.
pub fn encode_connect(out: &mut [u8], connect: &Connect<'_>) -> Result<usize, Error> {
    const CLEAN_SESSION: u8 = 0x02;
    const WILL_FLAG: u8 = 0x04;
    const WILL_RETAIN: u8 = 0x20;
    const PASSWORD_FLAG: u8 = 0x40;
    const USERNAME_FLAG: u8 = 0x80;

    let mut flags = CLEAN_SESSION;
    if let Some(will) = &connect.will {
        flags |= WILL_FLAG;
        if will.retain {
            flags |= WILL_RETAIN;
        }
    }
    if connect.user.is_some() {
        flags |= USERNAME_FLAG;
        if connect.pass.is_some() {
            flags |= PASSWORD_FLAG;
        }
    }

    // Variable header and payload are written first, into the tail of `out`,
    // so the remaining length is known before the fixed header goes down.
    let body_at = 1 + MAX_VARINT_LEN;
    if out.len() <= body_at {
        return Err(Error::BufferTooSmall);
    }
    let mut at = put_str(out, body_at, "MQTT")?;
    for byte in [PROTOCOL_LEVEL, flags] {
        *out.get_mut(at).ok_or(Error::BufferTooSmall)? = byte;
        at += 1;
    }
    let keepalive = connect.keepalive_s.to_be_bytes();
    for byte in keepalive {
        *out.get_mut(at).ok_or(Error::BufferTooSmall)? = byte;
        at += 1;
    }
    at = put_str(out, at, connect.client_id)?;
    if let Some(will) = &connect.will {
        at = put_str(out, at, will.topic)?;
        at = put_str(out, at, will.payload)?;
    }
    if let Some(user) = connect.user {
        at = put_str(out, at, user)?;
        if let Some(pass) = connect.pass {
            at = put_str(out, at, pass)?;
        }
    }

    finish(out, body_at, at, 0x10)
}

/// Encode a SUBSCRIBE packet for `filters`, all at QoS 0.
///
/// # Errors
///
/// As [`encode_connect`].
pub fn encode_subscribe(out: &mut [u8], packet_id: u16, filters: &[&str]) -> Result<usize, Error> {
    let body_at = 1 + MAX_VARINT_LEN;
    if out.len() <= body_at + 2 {
        return Err(Error::BufferTooSmall);
    }
    out[body_at..body_at + 2].copy_from_slice(&packet_id.to_be_bytes());
    let mut at = body_at + 2;
    for filter in filters {
        at = put_str(out, at, filter)?;
        *out.get_mut(at).ok_or(Error::BufferTooSmall)? = 0; // QoS 0
        at += 1;
    }
    // SUBSCRIBE is one of the packets whose low nibble is fixed at 0b0010.
    finish(out, body_at, at, 0x82)
}

/// Encode the fixed and variable header of a QoS 0 PUBLISH.
///
/// The caller writes `topic` and then the payload itself, so a large payload
/// never has to be copied into a packet buffer.
///
/// # Errors
///
/// As [`encode_connect`].
pub fn publish_header(
    out: &mut [u8; MAX_PUBLISH_HEADER],
    topic: &str,
    payload_len: usize,
    retain: bool,
) -> Result<usize, Error> {
    let topic_len = u16::try_from(topic.len()).map_err(|_| Error::TooLong)?;
    let remaining = u32::try_from(2 + topic.len() + payload_len).map_err(|_| Error::TooLong)?;
    out[0] = 0x30 | u8::from(retain);
    let digits = encode_varint(remaining, &mut out[1..])?;
    let at = 1 + digits;
    out[at..at + 2].copy_from_slice(&topic_len.to_be_bytes());
    Ok(at + 2)
}

/// Move a body written at `body_at..end` up against its fixed header.
fn finish(out: &mut [u8], body_at: usize, end: usize, header: u8) -> Result<usize, Error> {
    let remaining = u32::try_from(end - body_at).map_err(|_| Error::TooLong)?;
    let mut digits = [0u8; MAX_VARINT_LEN];
    let len = encode_varint(remaining, &mut digits)?;
    // The body sits at 1 + MAX_VARINT_LEN; a shorter length field means it
    // slides down by the difference.
    let shift = MAX_VARINT_LEN - len;
    if shift > 0 {
        out.copy_within(body_at..end, body_at - shift);
    }
    out[0] = header;
    out[1..1 + len].copy_from_slice(&digits[..len]);
    Ok(end - shift)
}

/// Check a CONNACK body (everything after the fixed header).
///
/// # Errors
///
/// [`Error::Malformed`] for a body that is not two bytes,
/// [`Error::ConnectionRefused`] for a non-zero return code.
pub fn parse_connack(body: &[u8]) -> Result<(), Error> {
    match body {
        [_, 0] => Ok(()),
        [_, code] => Err(Error::ConnectionRefused(*code)),
        _ => Err(Error::Malformed),
    }
}

/// Split an incoming PUBLISH body into its topic and payload.
///
/// `header` is the fixed header byte, needed for the QoS: at QoS 1 and 2 a
/// packet identifier sits between the topic and the payload. We subscribe at
/// QoS 0 only, but a broker that ignores that must not corrupt the parse.
///
/// # Errors
///
/// [`Error::Malformed`] for a truncated body, [`Error::NotUtf8`] for a topic
/// that is not UTF-8.
pub fn split_publish(header: u8, body: &[u8]) -> Result<(&str, &[u8]), Error> {
    let (topic, mut at) = take_str(body, 0)?;
    let qos = (header >> 1) & 0x03;
    if qos > 0 {
        at += 2;
    }
    let payload = body.get(at..).ok_or(Error::Malformed)?;
    Ok((topic, payload))
}

#[cfg(test)]
mod tests {
    use super::{
        decode_varint, encode_connect, encode_subscribe, encode_varint, parse_connack,
        publish_header, split_publish, Connect, Error, PacketType, Will, MAX_PUBLISH_HEADER,
    };

    #[test]
    fn varints_round_trip_at_the_boundaries() {
        for value in [0u32, 1, 127, 128, 16_383, 16_384, 2_097_151, 2_097_152] {
            let mut buf = [0u8; 4];
            let len = encode_varint(value, &mut buf).unwrap();
            assert_eq!(
                decode_varint(&buf[..len]),
                Ok(Some((value, len))),
                "{value}"
            );
        }
        assert_eq!(
            encode_varint(268_435_456, &mut [0u8; 4]),
            Err(Error::TooLong)
        );
        assert_eq!(
            encode_varint(300, &mut [0u8; 1]),
            Err(Error::BufferTooSmall)
        );
        // A valid prefix is "not yet", a five-byte run is malformed.
        assert_eq!(decode_varint(&[0x80]), Ok(None));
        assert_eq!(decode_varint(&[]), Ok(None));
        assert_eq!(
            decode_varint(&[0x80, 0x80, 0x80, 0x80]),
            Err(Error::Malformed)
        );
    }

    #[test]
    fn connect_matches_the_3_1_1_wire_format() {
        let mut buf = [0u8; 128];
        let len = encode_connect(
            &mut buf,
            &Connect {
                client_id: "wfi028t",
                keepalive_s: 60,
                user: None,
                pass: None,
                will: None,
            },
        )
        .unwrap();
        // 10 bytes of variable header + 2 + 7 of client id.
        assert_eq!(&buf[..12], b"\x10\x13\x00\x04MQTT\x04\x02\x00\x3c");
        assert_eq!(&buf[12..len], b"\x00\x07wfi028t");
        assert_eq!(len, 2 + 0x13);

        // With a will and credentials the flags gain will (0x04), will-retain
        // (0x20), password (0x40) and user name (0x80).
        let len = encode_connect(
            &mut buf,
            &Connect {
                client_id: "wfi028t",
                keepalive_s: 60,
                user: Some("ha"),
                pass: Some("pw"),
                will: Some(Will {
                    topic: "wfi028t/availability",
                    payload: "offline",
                    retain: true,
                }),
            },
        )
        .unwrap();
        assert_eq!(buf[9], 0x02 | 0x04 | 0x20 | 0x40 | 0x80);
        assert_eq!(usize::from(buf[1]) + 2, len);
        assert!(buf[..len].ends_with(b"\x00\x02ha\x00\x02pw"));
        // A password without a user name is not sent (the protocol forbids it).
        encode_connect(
            &mut buf,
            &Connect {
                client_id: "x",
                keepalive_s: 60,
                user: None,
                pass: Some("pw"),
                will: None,
            },
        )
        .unwrap();
        assert_eq!(buf[9] & 0x40, 0);

        assert_eq!(
            encode_connect(
                &mut [0u8; 16],
                &Connect {
                    client_id: "wfi028t",
                    keepalive_s: 60,
                    user: None,
                    pass: None,
                    will: None,
                }
            ),
            Err(Error::BufferTooSmall)
        );
    }

    #[test]
    fn long_packets_use_a_multi_byte_remaining_length() {
        // A client id long enough to push the remaining length over 127 proves
        // the body is moved up against the shorter header.
        let id = core::str::from_utf8(&[b'a'; 200]).unwrap();
        let mut buf = [0u8; 256];
        let len = encode_connect(
            &mut buf,
            &Connect {
                client_id: id,
                keepalive_s: 30,
                user: None,
                pass: None,
                will: None,
            },
        )
        .unwrap();
        assert_eq!(buf[0], 0x10);
        let (remaining, digits) = decode_varint(&buf[1..]).unwrap().unwrap();
        assert_eq!(digits, 2);
        assert_eq!(len, 1 + digits + remaining as usize);
        assert_eq!(&buf[1 + digits..1 + digits + 6], b"\x00\x04MQTT");
    }

    #[test]
    fn subscribe_carries_the_packet_id_and_qos_zero() {
        let mut buf = [0u8; 64];
        let len =
            encode_subscribe(&mut buf, 1, &["wfi028t/+/set", "homeassistant/status"]).unwrap();
        assert_eq!(buf[0], 0x82);
        assert_eq!(&buf[2..4], &[0x00, 0x01]);
        assert_eq!(&buf[4..6], &[0x00, 13]);
        assert_eq!(&buf[6..19], b"wfi028t/+/set");
        assert_eq!(buf[19], 0);
        assert_eq!(&buf[20..22], &[0x00, 20]);
        assert_eq!(buf[len - 1], 0);
        assert_eq!(usize::from(buf[1]) + 2, len);
    }

    #[test]
    fn publish_header_leaves_the_payload_to_the_caller() {
        let mut head = [0u8; MAX_PUBLISH_HEADER];
        let len = publish_header(&mut head, "wfi028t/state", 500, true).unwrap();
        assert_eq!(head[0], 0x31); // PUBLISH, retain
        let (remaining, digits) = decode_varint(&head[1..]).unwrap().unwrap();
        assert_eq!(remaining, 2 + 13 + 500);
        assert_eq!(len, 1 + digits + 2);
        assert_eq!(&head[1 + digits..len], &[0x00, 13]);

        let len = publish_header(&mut head, "a", 1, false).unwrap();
        assert_eq!(&head[..len], &[0x30, 4, 0x00, 1]);
    }

    #[test]
    fn connack_reports_the_brokers_verdict() {
        assert_eq!(parse_connack(&[0x00, 0x00]), Ok(()));
        assert_eq!(parse_connack(&[0x01, 0x00]), Ok(())); // session present
        assert_eq!(
            parse_connack(&[0x00, 0x05]),
            Err(Error::ConnectionRefused(5))
        );
        assert_eq!(parse_connack(&[0x00]), Err(Error::Malformed));
    }

    #[test]
    fn publish_splits_into_topic_and_payload() {
        let body = b"\x00\x11wfi028t/power/setON";
        assert_eq!(
            split_publish(0x30, body),
            Ok(("wfi028t/power/set", &b"ON"[..]))
        );
        // QoS 1: two bytes of packet identifier before the payload.
        let body = b"\x00\x01a\x12\x34ON";
        assert_eq!(split_publish(0x32, body), Ok(("a", &b"ON"[..])));
        // An empty payload is legal and is how a retained message is cleared.
        assert_eq!(split_publish(0x30, b"\x00\x01a"), Ok(("a", &[][..])));
        assert_eq!(split_publish(0x30, b"\x00\x05ab"), Err(Error::Malformed));
        assert_eq!(split_publish(0x30, b"\x00\x01\xff"), Err(Error::NotUtf8));
    }

    #[test]
    fn header_bytes_classify() {
        assert_eq!(PacketType::from_header(0x20), PacketType::Connack);
        assert_eq!(PacketType::from_header(0x31), PacketType::Publish);
        assert_eq!(PacketType::from_header(0x90), PacketType::Suback);
        assert_eq!(PacketType::from_header(0xd0), PacketType::Pingresp);
        assert_eq!(PacketType::from_header(0x40), PacketType::Other(4));
    }
}
