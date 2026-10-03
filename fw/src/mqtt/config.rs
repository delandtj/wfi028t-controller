//! Broker configuration: the value type, its build-time default, its
//! on-flash record and the parsing of the `mqtt` line commands. Pure: no
//! I/O, no clock, no allocation (see [`super::json`] for how it is tested on
//! the host).
//!
//! Two sources, runtime wins:
//!
//! | Source | Set by |
//! |---|---|
//! | build time | `MQTT_HOST`, `MQTT_PORT`, `MQTT_USER`, `MQTT_PASS` in the environment of `cargo build` |
//! | runtime | `mqtt host <ip> [port]`, `mqtt user <u> <p>`, `mqtt off`, kept in the `nvs` partition |
//!
//! [`Stored::Unset`] - the state of a board that was never configured over
//! the wire - falls back to the build-time values, so an image built with a
//! broker address works out of the box and a field change still sticks
//! across reboots. [`Stored::Off`] is an explicit "no MQTT", which is NOT
//! the same as unset: it overrides the build-time default.
//!
//! The password is held as bytes in a struct that deliberately has no
//! `Debug`, and nothing in this firmware formats it.

use hp_model::rtu::crc16;

/// Default MQTT port, used when the configuration names none.
pub const DEFAULT_PORT: u16 = 1883;

/// Longest user name that fits the flash record.
pub const MAX_USER: usize = 32;

/// Longest password that fits the flash record.
pub const MAX_PASS: usize = 64;

/// Broker address from the environment at build time (an IPv4 literal; this
/// firmware does no DNS).
const BUILD_HOST: Option<&str> = option_env!("MQTT_HOST");
/// Broker port at build time; [`DEFAULT_PORT`] if absent or unparsable.
const BUILD_PORT: Option<&str> = option_env!("MQTT_PORT");
/// Broker user name at build time.
const BUILD_USER: Option<&str> = option_env!("MQTT_USER");
/// Broker password at build time.
const BUILD_PASS: Option<&str> = option_env!("MQTT_PASS");

/// Where and how to reach the broker.
///
/// `Copy` and free of references so it can live in a `Cell` behind a
/// critical-section mutex, which is what lets a command handler swap it
/// without awaiting anything. No `Debug`: it holds the password.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Broker {
    host: [u8; 4],
    port: u16,
    user: [u8; MAX_USER],
    user_len: u8,
    pass: [u8; MAX_PASS],
    pass_len: u8,
}

/// Shows the address and the user name; the password is never formatted,
/// which is the whole reason this is not a derive.
impl core::fmt::Debug for Broker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let [a, b, c, d] = self.host;
        write!(
            f,
            "Broker({a}.{b}.{c}.{d}:{}, user {:?}, password {})",
            self.port,
            self.user(),
            if self.pass_len > 0 { "set" } else { "unset" }
        )
    }
}

impl Broker {
    /// Build a configuration. `None` if the credentials do not fit.
    #[must_use]
    pub fn new(host: [u8; 4], port: u16, user: &str, pass: &str) -> Option<Self> {
        if user.len() > MAX_USER || pass.len() > MAX_PASS {
            return None;
        }
        let mut broker = Self {
            host,
            port: if port == 0 { DEFAULT_PORT } else { port },
            user: [0; MAX_USER],
            user_len: user.len() as u8,
            pass: [0; MAX_PASS],
            pass_len: pass.len() as u8,
        };
        broker.user[..user.len()].copy_from_slice(user.as_bytes());
        broker.pass[..pass.len()].copy_from_slice(pass.as_bytes());
        Some(broker)
    }

    /// The same broker with different credentials. `None` if they do not fit.
    #[must_use]
    pub fn with_credentials(&self, user: &str, pass: &str) -> Option<Self> {
        Self::new(self.host, self.port, user, pass)
    }

    /// The same broker at a different address.
    #[must_use]
    pub fn with_address(&self, host: [u8; 4], port: u16) -> Self {
        Self {
            host,
            port: if port == 0 { DEFAULT_PORT } else { port },
            ..*self
        }
    }

    /// IPv4 address of the broker.
    #[must_use]
    pub const fn host(&self) -> [u8; 4] {
        self.host
    }

    /// TCP port of the broker.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// User name, empty if the broker needs none.
    #[must_use]
    pub fn user(&self) -> &str {
        // Only ever filled from a `&str`, so this cannot fail; an empty name
        // is the honest answer if it somehow did.
        core::str::from_utf8(&self.user[..usize::from(self.user_len)]).unwrap_or("")
    }

    /// Password, empty if the broker needs none. Never logged or published.
    #[must_use]
    pub fn pass(&self) -> &str {
        core::str::from_utf8(&self.pass[..usize::from(self.pass_len)]).unwrap_or("")
    }

    /// The user name as MQTT wants it: absent rather than empty.
    #[must_use]
    pub fn user_opt(&self) -> Option<&str> {
        (self.user_len > 0).then(|| self.user())
    }

    /// The password as MQTT wants it: absent rather than empty.
    #[must_use]
    pub fn pass_opt(&self) -> Option<&str> {
        (self.pass_len > 0).then(|| self.pass())
    }
}

/// What the flash record says, which is not the same as what is in force
/// (see [`effective`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// No record: the build-time configuration applies.
    Unset,
    /// `mqtt off`: no MQTT, whatever the build said.
    Off,
    /// A broker configured over the wire.
    On(Broker),
}

/// The broker in force: the stored one, or the build-time one if nothing was
/// ever stored. `None` means "do not connect".
#[must_use]
pub fn effective(stored: Stored) -> Option<Broker> {
    match stored {
        Stored::On(broker) => Some(broker),
        Stored::Off => None,
        Stored::Unset => build_default(),
    }
}

/// The build-time configuration, if `MQTT_HOST` was set and parses.
#[must_use]
pub fn build_default() -> Option<Broker> {
    let host = parse_ipv4(BUILD_HOST?)?;
    let port = BUILD_PORT
        .and_then(|text| text.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    Broker::new(
        host,
        port,
        BUILD_USER.unwrap_or(""),
        BUILD_PASS.unwrap_or(""),
    )
}

/// Parse a dotted-quad IPv4 literal. Strict: four decimal octets, nothing
/// else, no leading `+`, no whitespace.
#[must_use]
pub fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = text.split('.');
    for octet in &mut octets {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *octet = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(octets)
}

// ---------------------------------------------------------------------------
// Flash record
// ---------------------------------------------------------------------------

/// Size of the on-flash broker record. A multiple of the flash word size (4).
pub const RECORD_LEN: usize = 112;

/// Magic at the start of the record: "WCQ1" (Wfi Controller mQtt, rev 1).
const RECORD_MAGIC: u32 = u32::from_le_bytes(*b"WCQ1");

/// Record format version. Bump when the layout changes.
const RECORD_VERSION: u8 = 1;

/// Serialise the broker setting into its on-flash form.
///
/// Layout, little endian:
///
/// | Offset | Bytes | Field |
/// |---|---|---|
/// | 0 | 4 | magic |
/// | 4 | 1 | version |
/// | 5 | 1 | 0 = off, 1 = on |
/// | 6 | 2 | port |
/// | 8 | 4 | IPv4 address |
/// | 12 | 1 | user name length |
/// | 13 | 1 | password length |
/// | 14 | 32 | user name |
/// | 46 | 64 | password |
/// | 110 | 2 | CRC-16 of bytes 0..110 |
///
/// [`Stored::Unset`] has no representation: not writing a record (or writing
/// one that fails the CRC) is what "unset" means.
#[must_use]
pub fn encode_record(stored: Stored) -> Option<[u8; RECORD_LEN]> {
    let mut raw = [0u8; RECORD_LEN];
    raw[0..4].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
    raw[4] = RECORD_VERSION;
    match stored {
        Stored::Unset => return None,
        Stored::Off => raw[5] = 0,
        Stored::On(broker) => {
            raw[5] = 1;
            raw[6..8].copy_from_slice(&broker.port.to_le_bytes());
            raw[8..12].copy_from_slice(&broker.host);
            raw[12] = broker.user_len;
            raw[13] = broker.pass_len;
            raw[14..14 + MAX_USER].copy_from_slice(&broker.user);
            raw[46..46 + MAX_PASS].copy_from_slice(&broker.pass);
        }
    }
    let crc = crc16(&raw[0..RECORD_LEN - 2]);
    raw[RECORD_LEN - 2..].copy_from_slice(&crc.to_le_bytes());
    Some(raw)
}

/// Parse an on-flash broker record. [`Stored::Unset`] for anything that is
/// not a valid, current record - including erased (all-0xff) flash.
#[must_use]
pub fn decode_record(raw: &[u8]) -> Stored {
    if raw.len() < RECORD_LEN
        || u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) != RECORD_MAGIC
        || raw[4] != RECORD_VERSION
        || u16::from_le_bytes([raw[RECORD_LEN - 2], raw[RECORD_LEN - 1]])
            != crc16(&raw[0..RECORD_LEN - 2])
    {
        return Stored::Unset;
    }
    if raw[5] == 0 {
        return Stored::Off;
    }
    let user_len = usize::from(raw[12]);
    let pass_len = usize::from(raw[13]);
    if user_len > MAX_USER || pass_len > MAX_PASS {
        return Stored::Unset;
    }
    let host = [raw[8], raw[9], raw[10], raw[11]];
    let port = u16::from_le_bytes([raw[6], raw[7]]);
    let user = core::str::from_utf8(&raw[14..14 + user_len]);
    let pass = core::str::from_utf8(&raw[46..46 + pass_len]);
    match (user, pass) {
        (Ok(user), Ok(pass)) => {
            Broker::new(host, port, user, pass).map_or(Stored::Unset, Stored::On)
        }
        _ => Stored::Unset,
    }
}

// ---------------------------------------------------------------------------
// The `mqtt` line command
// ---------------------------------------------------------------------------

/// What an `mqtt ...` command asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// `mqtt`: report the current configuration and connection state.
    Report,
    /// `mqtt host <ip> [port]`: address, keeping the credentials.
    Address([u8; 4], u16),
    /// `mqtt user <u> <p>`: credentials, keeping the address. Borrowed from
    /// the command line, so the password is never copied into an
    /// intermediate buffer on its way to [`Broker`].
    Credentials(&'a str, &'a str),
    /// `mqtt off`.
    Off,
}

/// Usage text, also the error reply for a malformed command.
pub const USAGE: &str = "usage: mqtt [host <ip> [port] | user <name> <password> | off]";

/// Parse the argument part of an `mqtt` command.
///
/// # Errors
///
/// [`USAGE`], or a more specific reason, for anything that is not one of the
/// four forms.
pub fn parse_request(rest: &str) -> Result<Request<'_>, &'static str> {
    if rest.is_empty() {
        return Ok(Request::Report);
    }
    let (word, args) = split_word(rest);
    if word.eq_ignore_ascii_case("off") && args.is_empty() {
        return Ok(Request::Off);
    }
    if word.eq_ignore_ascii_case("host") {
        let (ip, port) = split_word(args);
        let host = parse_ipv4(ip).ok_or("not an IPv4 address")?;
        let port = if port.is_empty() {
            DEFAULT_PORT
        } else {
            port.parse::<u16>().map_err(|_| "not a port number")?
        };
        if port == 0 {
            return Err("not a port number");
        }
        return Ok(Request::Address(host, port));
    }
    if word.eq_ignore_ascii_case("user") {
        let (user, pass) = credentials(args)?;
        return Ok(Request::Credentials(user, pass));
    }
    Err(USAGE)
}

/// The user name and password of an `mqtt user <u> <p>` command.
///
/// # Errors
///
/// A reason for a missing field or one that does not fit the flash record.
pub fn credentials(args: &str) -> Result<(&str, &str), &'static str> {
    let (user, pass) = split_word(args);
    if user.is_empty() || pass.is_empty() {
        return Err("usage: mqtt user <name> <password>");
    }
    if pass.contains([' ', '\t']) {
        return Err("password must not contain spaces");
    }
    if user.len() > MAX_USER {
        return Err("user name too long");
    }
    if pass.len() > MAX_PASS {
        return Err("password too long");
    }
    Ok((user, pass))
}

/// Split off the first whitespace-separated word.
fn split_word(text: &str) -> (&str, &str) {
    match text.find([' ', '\t']) {
        Some(i) => (&text[..i], text[i + 1..].trim_matches([' ', '\t'])),
        None => (text, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        credentials, decode_record, effective, encode_record, parse_ipv4, parse_request, Broker,
        Request, Stored, DEFAULT_PORT, MAX_PASS, MAX_USER, RECORD_LEN,
    };

    fn broker() -> Broker {
        Broker::new([192, 168, 64, 10], 1883, "ha", "secret").unwrap()
    }

    #[test]
    fn ipv4_parsing_is_strict() {
        assert_eq!(parse_ipv4("192.168.64.10"), Some([192, 168, 64, 10]));
        assert_eq!(parse_ipv4("0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4("255.255.255.255"), Some([255, 255, 255, 255]));
        for bad in [
            "192.168.64",
            "192.168.64.10.1",
            "192.168.64.256",
            "192.168.64.",
            ".1.2.3",
            "192.168.64.10 ",
            "192.168.64.+1",
            "broker.local",
            "",
        ] {
            assert_eq!(parse_ipv4(bad), None, "{bad}");
        }
    }

    #[test]
    fn credentials_round_trip_and_are_bounded() {
        let b = broker();
        assert_eq!(b.user(), "ha");
        assert_eq!(b.pass(), "secret");
        assert_eq!(b.user_opt(), Some("ha"));
        assert_eq!(b.pass_opt(), Some("secret"));
        assert_eq!(b.host(), [192, 168, 64, 10]);
        assert_eq!(b.port(), 1883);

        let anonymous = Broker::new([10, 0, 0, 1], 0, "", "").unwrap();
        assert_eq!(anonymous.port(), DEFAULT_PORT);
        assert_eq!(anonymous.user_opt(), None);
        assert_eq!(anonymous.pass_opt(), None);

        let long = core::str::from_utf8(&[b'x'; MAX_USER + 1]).unwrap();
        assert!(Broker::new([10, 0, 0, 1], 1883, long, "").is_none());
        let long = core::str::from_utf8(&[b'x'; MAX_PASS + 1]).unwrap();
        assert!(Broker::new([10, 0, 0, 1], 1883, "ha", long).is_none());

        let moved = b.with_address([10, 1, 2, 3], 8883);
        assert_eq!(moved.host(), [10, 1, 2, 3]);
        assert_eq!(moved.port(), 8883);
        assert_eq!(moved.user(), "ha");
        let recredentialed = b.with_credentials("bob", "pw").unwrap();
        assert_eq!(recredentialed.host(), b.host());
        assert_eq!(recredentialed.user(), "bob");
        assert_eq!(recredentialed.pass(), "pw");
    }

    #[test]
    fn runtime_overrides_build_time() {
        // The test harness builds without MQTT_HOST, so the build default is
        // "no broker" and Unset means no MQTT.
        assert!(super::build_default().is_none());
        assert!(effective(Stored::Unset).is_none());
        assert!(effective(Stored::Off).is_none());
        assert_eq!(effective(Stored::On(broker())), Some(broker()));
    }

    #[test]
    fn the_flash_record_round_trips() {
        let raw = encode_record(Stored::On(broker())).unwrap();
        assert_eq!(raw.len(), RECORD_LEN);
        assert_eq!(decode_record(&raw), Stored::On(broker()));

        let raw = encode_record(Stored::Off).unwrap();
        assert_eq!(decode_record(&raw), Stored::Off);

        // Unset has no record at all.
        assert!(encode_record(Stored::Unset).is_none());

        // Erased flash, a foreign record and a flipped bit all read as unset.
        assert_eq!(decode_record(&[0xff; RECORD_LEN]), Stored::Unset);
        assert_eq!(decode_record(&[0x00; RECORD_LEN]), Stored::Unset);
        assert_eq!(decode_record(&[]), Stored::Unset);
        let mut raw = encode_record(Stored::On(broker())).unwrap();
        assert_eq!(decode_record(&raw[..RECORD_LEN - 1]), Stored::Unset);
        raw[20] ^= 0x01;
        assert_eq!(decode_record(&raw), Stored::Unset);
        // A version from the future is not guessed at.
        let mut raw = encode_record(Stored::Off).unwrap();
        raw[4] = 2;
        assert_eq!(decode_record(&raw), Stored::Unset);

        // The longest credentials still fit.
        let user = core::str::from_utf8(&[b'u'; MAX_USER]).unwrap();
        let pass = core::str::from_utf8(&[b'p'; MAX_PASS]).unwrap();
        let full = Stored::On(Broker::new([1, 2, 3, 4], 8883, user, pass).unwrap());
        assert_eq!(decode_record(&encode_record(full).unwrap()), full);
    }

    #[test]
    fn commands_parse() {
        assert!(matches!(parse_request(""), Ok(Request::Report)));
        assert!(matches!(parse_request("off"), Ok(Request::Off)));
        assert_eq!(
            parse_request("host 192.168.64.10"),
            Ok(Request::Address([192, 168, 64, 10], DEFAULT_PORT))
        );
        assert_eq!(
            parse_request("host 192.168.64.10 8883"),
            Ok(Request::Address([192, 168, 64, 10], 8883))
        );
        assert_eq!(
            parse_request("user ha secret"),
            Ok(Request::Credentials("ha", "secret"))
        );
        assert_eq!(credentials("ha secret"), Ok(("ha", "secret")));

        assert!(parse_request("host").is_err());
        assert!(parse_request("host nope").is_err());
        assert!(parse_request("host 192.168.64.10 0").is_err());
        assert!(parse_request("host 192.168.64.10 70000").is_err());
        assert!(parse_request("user").is_err());
        assert!(parse_request("user ha").is_err());
        assert!(parse_request("user ha pass word").is_err());
        assert!(parse_request("off now").is_err());
        assert!(parse_request("nonsense").is_err());
    }
}
