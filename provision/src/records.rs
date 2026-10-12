//! The three records ADR 0004 adds to the settings sector: WiFi station
//! credentials, the admin password and the device's own identity.
//!
//! Each one is built the way the bus, mode and MQTT records already are: a
//! magic word, a format version, the fields, and a Modbus CRC-16 over
//! everything before it. Erased flash (all 0xff), a record from a future
//! version, a foreign magic and a flipped bit all decode to `None`, so the
//! firmware falls back to its build-time defaults instead of guessing - the
//! same contract [`crate::layout`] documents for the older records.
//!
//! | Record | Bytes | Magic | Value |
//! |---|---|---|---|
//! | WiFi | 112 | `WCW1` | [`Wifi`] |
//! | admin | 64 | `WCA1` | [`Admin`] |
//! | device | 48 | `WCD1` | [`Device`] |
//!
//! The WiFi password is stored in clear: flash encryption is off and anyone
//! with the board in hand can read the sector anyway (ADR 0004, "Why not
//! store the WiFi password encrypted?"). What this module can do is make it
//! hard to leak by accident, so [`Wifi`] has a hand-written [`core::fmt::Debug`]
//! that never formats it and no accessor returns it by `Display`.
//!
//! The CRC is `hp_model::rtu::crc16`, which is what the MQTT record already
//! uses; `modbus_sniffer_core::crc16` is the same algorithm, so a dump tool
//! needs only one implementation.

use hp_model::rtu::crc16;

/// Longest SSID that fits the WiFi record (and the 802.11 maximum).
pub const MAX_SSID: usize = 32;

/// Longest WiFi password that fits the record. A WPA2 passphrase is at most
/// 63 characters, a raw PSK is 64 hex digits.
pub const MAX_WIFI_PASS: usize = 64;

/// Longest hostname that fits the device record.
pub const MAX_HOSTNAME: usize = 32;

/// Length of the PBKDF2 salt (ADR 0003).
pub const SALT_LEN: usize = 16;

/// Length of the PBKDF2-HMAC-SHA256 hash (ADR 0003).
pub const HASH_LEN: usize = 32;

/// Size of the on-flash WiFi record. A multiple of the flash word size (4).
pub const WIFI_RECORD_LEN: usize = 112;

/// Size of the on-flash admin record.
pub const ADMIN_RECORD_LEN: usize = 64;

/// Size of the on-flash device record.
pub const DEVICE_RECORD_LEN: usize = 48;

/// Magic at the start of the WiFi record: "WCW1" (Wfi Controller Wifi, rev 1).
const WIFI_MAGIC: u32 = u32::from_le_bytes(*b"WCW1");

/// Magic at the start of the admin record: "WCA1" (Admin, rev 1).
const ADMIN_MAGIC: u32 = u32::from_le_bytes(*b"WCA1");

/// Magic at the start of the device record: "WCD1" (Device, rev 1).
const DEVICE_MAGIC: u32 = u32::from_le_bytes(*b"WCD1");

/// Record format version, one per record. Bump when a layout changes.
const VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// Shared record frame
// ---------------------------------------------------------------------------

/// Write the magic and version at the front of a zeroed record body.
fn open(raw: &mut [u8], magic: u32) {
    raw[0..4].copy_from_slice(&magic.to_le_bytes());
    raw[4] = VERSION;
}

/// Append the CRC-16 over everything before it, which ends the record.
fn seal(raw: &mut [u8]) {
    let end = raw.len() - 2;
    let crc = crc16(&raw[..end]);
    raw[end..].copy_from_slice(&crc.to_le_bytes());
}

/// Whether `raw` is a current record of this kind: long enough, the right
/// magic, a version this firmware knows, and a CRC that matches.
fn framed(raw: &[u8], magic: u32, len: usize) -> bool {
    raw.len() >= len
        && u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) == magic
        && raw[4] == VERSION
        && u16::from_le_bytes([raw[len - 2], raw[len - 1]]) == crc16(&raw[..len - 2])
}

/// A fixed-length field as text, or `None` if the length is out of range or
/// the bytes are not UTF-8.
fn text(raw: &[u8], at: usize, len: usize, max: usize) -> Option<&str> {
    if len > max {
        return None;
    }
    core::str::from_utf8(&raw[at..at + len]).ok()
}

/// Reject text that cannot survive the line protocol or a web form.
fn printable(text: &str, what: &'static str) -> Result<(), &'static str> {
    if text.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(what);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// WiFi
// ---------------------------------------------------------------------------

/// Stored station credentials. They win over the build-time
/// `SNIFFER_WIFI_SSID` / `SNIFFER_WIFI_PASS`, which stay as the default so a
/// bench build still boots onto the bench network (ADR 0004, component 1).
///
/// `Copy` and free of references, so the firmware can hold it in a static
/// without an allocator. No derived `Debug`: it holds the password.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Wifi {
    ssid: [u8; MAX_SSID],
    ssid_len: u8,
    pass: [u8; MAX_WIFI_PASS],
    pass_len: u8,
    /// Reserved for per-network options a later ADR may need (hidden SSID, a
    /// fixed channel). Written as zero and ignored on read; a meaning for a
    /// bit comes with a version bump.
    flags: u8,
}

/// Shows the SSID and whether a password is set, never the password itself -
/// which is the whole reason this is not a derive.
impl core::fmt::Debug for Wifi {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Wifi({:?}, password {})",
            self.ssid(),
            if self.pass_len > 0 { "set" } else { "unset" }
        )
    }
}

impl Wifi {
    /// Credentials for one network.
    ///
    /// # Errors
    ///
    /// Protocol-visible text for an empty or over-long SSID, an over-long
    /// password, or either field holding a control character.
    pub fn new(ssid: &str, pass: &str) -> Result<Self, &'static str> {
        if ssid.is_empty() {
            return Err("SSID must not be empty");
        }
        if ssid.len() > MAX_SSID {
            return Err("SSID too long (32 bytes)");
        }
        if pass.len() > MAX_WIFI_PASS {
            return Err("WiFi password too long (64 bytes)");
        }
        printable(ssid, "SSID must be printable")?;
        printable(pass, "WiFi password must be printable")?;

        let mut wifi = Self {
            ssid: [0; MAX_SSID],
            ssid_len: ssid.len() as u8,
            pass: [0; MAX_WIFI_PASS],
            pass_len: pass.len() as u8,
            flags: 0,
        };
        wifi.ssid[..ssid.len()].copy_from_slice(ssid.as_bytes());
        wifi.pass[..pass.len()].copy_from_slice(pass.as_bytes());
        Ok(wifi)
    }

    /// The network name.
    #[must_use]
    pub fn ssid(&self) -> &str {
        // Only ever filled from a `&str` or a record that was checked on the
        // way in, so this cannot fail.
        core::str::from_utf8(&self.ssid[..usize::from(self.ssid_len)]).unwrap_or("")
    }

    /// The passphrase, empty for an open network. Never published or logged.
    #[must_use]
    pub fn pass(&self) -> &str {
        core::str::from_utf8(&self.pass[..usize::from(self.pass_len)]).unwrap_or("")
    }

    /// Whether a passphrase is set, for a report that must not show it.
    #[must_use]
    pub const fn has_pass(&self) -> bool {
        self.pass_len > 0
    }
}

/// Serialise the WiFi record.
///
/// Layout, little endian:
///
/// | Offset | Bytes | Field |
/// |---|---|---|
/// | 0 | 4 | magic |
/// | 4 | 1 | version |
/// | 5 | 1 | flags (reserved, zero) |
/// | 6 | 1 | SSID length |
/// | 7 | 1 | password length |
/// | 8 | 32 | SSID |
/// | 40 | 64 | password |
/// | 104 | 6 | reserved, zero |
/// | 110 | 2 | CRC-16 of bytes 0..110 |
#[must_use]
pub fn encode_wifi(wifi: &Wifi) -> [u8; WIFI_RECORD_LEN] {
    let mut raw = [0u8; WIFI_RECORD_LEN];
    open(&mut raw, WIFI_MAGIC);
    raw[5] = wifi.flags;
    raw[6] = wifi.ssid_len;
    raw[7] = wifi.pass_len;
    raw[8..8 + MAX_SSID].copy_from_slice(&wifi.ssid);
    raw[40..40 + MAX_WIFI_PASS].copy_from_slice(&wifi.pass);
    seal(&mut raw);
    raw
}

/// Parse the WiFi record. `None` for erased flash and for anything that is
/// not a valid, current record.
#[must_use]
pub fn decode_wifi(raw: &[u8]) -> Option<Wifi> {
    if !framed(raw, WIFI_MAGIC, WIFI_RECORD_LEN) {
        return None;
    }
    let ssid = text(raw, 8, usize::from(raw[6]), MAX_SSID)?;
    let pass = text(raw, 40, usize::from(raw[7]), MAX_WIFI_PASS)?;
    let mut wifi = Wifi::new(ssid, pass).ok()?;
    wifi.flags = raw[5];
    Some(wifi)
}

/// Usage text for the `wifi` command, also its error reply.
pub const WIFI_USAGE: &str =
    "usage: wifi [set <ssid> <password> | clear] (quote a field with spaces: \"my network\")";

/// Split the arguments of `wifi set <ssid> <password>`.
///
/// A field that contains spaces has to be wrapped in double quotes:
///
/// ```text
/// wifi set bench secret
/// wifi set "my network" "pass phrase"
/// ```
///
/// There is no escape character, so neither field can contain a double
/// quote - which no sane SSID does, and a passphrase that needs one can be
/// set from the web page (ADR 0004, component 5) instead. An empty password
/// (`wifi set open ""`) means an open network.
///
/// # Errors
///
/// [`WIFI_USAGE`] for a missing field, an unterminated quote or trailing
/// junk after the password.
pub fn parse_wifi_set(rest: &str) -> Result<(&str, &str), &'static str> {
    let (ssid, rest) = field(rest).ok_or(WIFI_USAGE)?;
    let (pass, rest) = field(rest).ok_or(WIFI_USAGE)?;
    if !rest.is_empty() {
        return Err(WIFI_USAGE);
    }
    Ok((ssid, pass))
}

/// One field and what follows it: a double-quoted run, or everything up to
/// the next space. `None` if there is no field or the quote is unterminated.
fn field(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start_matches([' ', '\t']);
    if let Some(quoted) = text.strip_prefix('"') {
        let end = quoted.find('"')?;
        let rest = quoted[end + 1..].trim_start_matches([' ', '\t']);
        return Some((&quoted[..end], rest));
    }
    if text.is_empty() {
        return None;
    }
    match text.find([' ', '\t']) {
        Some(i) => Some((&text[..i], text[i + 1..].trim_start_matches([' ', '\t']))),
        None => Some((text, "")),
    }
}

// ---------------------------------------------------------------------------
// Admin password
// ---------------------------------------------------------------------------

/// The admin password as ADR 0003 stores it: a salt, an iteration count and
/// the PBKDF2-HMAC-SHA256 output. The password itself is never stored, and
/// no record at all means no password is set - which is what makes a device
/// "unconfigured" and sends it into setup mode.
///
/// This module only moves the bytes; producing and checking them is ADR
/// 0003's job.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Admin {
    salt: [u8; SALT_LEN],
    iterations: u32,
    hash: [u8; HASH_LEN],
}

/// Shows the iteration count only: the salt and hash are not secrets, but
/// printing them invites a log that helps an offline guess.
impl core::fmt::Debug for Admin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Admin({} iterations)", self.iterations)
    }
}

impl Admin {
    /// A stored password.
    ///
    /// # Errors
    ///
    /// Protocol-visible text if the iteration count is zero, which would be
    /// a hash nobody can have produced on purpose.
    pub fn new(
        salt: [u8; SALT_LEN],
        iterations: u32,
        hash: [u8; HASH_LEN],
    ) -> Result<Self, &'static str> {
        if iterations == 0 {
            return Err("iteration count must not be zero");
        }
        Ok(Self {
            salt,
            iterations,
            hash,
        })
    }

    /// The PBKDF2 salt.
    #[must_use]
    pub const fn salt(&self) -> &[u8; SALT_LEN] {
        &self.salt
    }

    /// The PBKDF2 iteration count.
    #[must_use]
    pub const fn iterations(&self) -> u32 {
        self.iterations
    }

    /// The stored hash, to compare a login attempt against.
    #[must_use]
    pub const fn hash(&self) -> &[u8; HASH_LEN] {
        &self.hash
    }
}

/// Serialise the admin record.
///
/// Layout, little endian:
///
/// | Offset | Bytes | Field |
/// |---|---|---|
/// | 0 | 4 | magic |
/// | 4 | 1 | version |
/// | 5 | 3 | reserved, zero |
/// | 8 | 4 | PBKDF2 iterations |
/// | 12 | 16 | salt |
/// | 28 | 32 | hash |
/// | 60 | 2 | reserved, zero |
/// | 62 | 2 | CRC-16 of bytes 0..62 |
#[must_use]
pub fn encode_admin(admin: &Admin) -> [u8; ADMIN_RECORD_LEN] {
    let mut raw = [0u8; ADMIN_RECORD_LEN];
    open(&mut raw, ADMIN_MAGIC);
    raw[8..12].copy_from_slice(&admin.iterations.to_le_bytes());
    raw[12..12 + SALT_LEN].copy_from_slice(&admin.salt);
    raw[28..28 + HASH_LEN].copy_from_slice(&admin.hash);
    seal(&mut raw);
    raw
}

/// Parse the admin record. `None` for erased flash and for anything that is
/// not a valid, current record - and therefore "no password set".
#[must_use]
pub fn decode_admin(raw: &[u8]) -> Option<Admin> {
    if !framed(raw, ADMIN_MAGIC, ADMIN_RECORD_LEN) {
        return None;
    }
    let iterations = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&raw[12..12 + SALT_LEN]);
    let mut hash = [0u8; HASH_LEN];
    hash.copy_from_slice(&raw[28..28 + HASH_LEN]);
    Admin::new(salt, iterations, hash).ok()
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// What the line ports (4000 and 4001) accept, per ADR 0004's table. USB
/// always gets [`LinePolicy::Full`]: it needs physical access.
///
/// Stored, not yet enforced: the ports behave as they always did until the
/// guard is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinePolicy {
    /// Every command, as today.
    Full,
    /// The stream and `status`; anything else is refused.
    ReadOnly,
    /// The ports are closed.
    Off,
}

impl LinePolicy {
    /// The on-flash code for this policy.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Full => 0,
            Self::ReadOnly => 1,
            Self::Off => 2,
        }
    }

    /// The policy an on-flash code names, `None` for one this firmware does
    /// not know.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Full),
            1 => Some(Self::ReadOnly),
            2 => Some(Self::Off),
            _ => None,
        }
    }

    /// The name used on the console, in `status` and on the web page.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ReadOnly => "read-only",
            Self::Off => "off",
        }
    }

    /// Parse the name a command or form field carries.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("full") {
            Some(Self::Full)
        } else if text.eq_ignore_ascii_case("read-only") || text.eq_ignore_ascii_case("readonly") {
            Some(Self::ReadOnly)
        } else if text.eq_ignore_ascii_case("off") {
            Some(Self::Off)
        } else {
            None
        }
    }
}

/// The device's own settings: the name it asks DHCP for and answers mDNS on,
/// what the line ports accept, and whether the setup wizard ever finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    hostname: [u8; MAX_HOSTNAME],
    hostname_len: u8,
    ports: LinePolicy,
    setup_complete: bool,
}

impl Device {
    /// The device record. A stored hostname wins over the build-time
    /// `SNIFFER_HOSTNAME` and over `wfi028t-XXXX` from the MAC.
    ///
    /// # Errors
    ///
    /// Protocol-visible text from [`check_hostname`].
    pub fn new(
        hostname: &str,
        ports: LinePolicy,
        setup_complete: bool,
    ) -> Result<Self, &'static str> {
        check_hostname(hostname)?;
        let mut device = Self {
            hostname: [0; MAX_HOSTNAME],
            hostname_len: hostname.len() as u8,
            ports,
            setup_complete,
        };
        device.hostname[..hostname.len()].copy_from_slice(hostname.as_bytes());
        Ok(device)
    }

    /// The same record under a different name.
    ///
    /// # Errors
    ///
    /// Protocol-visible text from [`check_hostname`].
    pub fn with_hostname(&self, hostname: &str) -> Result<Self, &'static str> {
        Self::new(hostname, self.ports, self.setup_complete)
    }

    /// The hostname.
    #[must_use]
    pub fn hostname(&self) -> &str {
        // Checked on the way in, from a `&str` or a record, so this holds.
        core::str::from_utf8(&self.hostname[..usize::from(self.hostname_len)]).unwrap_or("")
    }

    /// What the line ports accept. Stored only, for now.
    #[must_use]
    pub const fn ports(&self) -> LinePolicy {
        self.ports
    }

    /// Whether the setup wizard ever reached its last page.
    #[must_use]
    pub const fn setup_complete(&self) -> bool {
        self.setup_complete
    }
}

/// Whether `hostname` is one a DHCP server and an mDNS responder will carry:
/// 1 to 32 bytes of `a-z`, `0-9` and `-`, not starting or ending with `-`.
///
/// Strict on purpose, and lower case only: the name ends up in a DHCP option
/// 12, in `<name>.local` queries and in the capture tooling's bookmarks, and
/// a name that survives all three is worth more than one that looks nicer in
/// a form.
///
/// # Errors
///
/// Protocol-visible text naming the rule that was broken.
pub fn check_hostname(hostname: &str) -> Result<(), &'static str> {
    if hostname.is_empty() {
        return Err("hostname must not be empty");
    }
    if hostname.len() > MAX_HOSTNAME {
        return Err("hostname too long (32 bytes)");
    }
    if !hostname
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("hostname may only use a-z, 0-9 and '-'");
    }
    if hostname.starts_with('-') || hostname.ends_with('-') {
        return Err("hostname must not start or end with '-'");
    }
    Ok(())
}

/// Serialise the device record.
///
/// Layout, little endian:
///
/// | Offset | Bytes | Field |
/// |---|---|---|
/// | 0 | 4 | magic |
/// | 4 | 1 | version |
/// | 5 | 1 | hostname length |
/// | 6 | 1 | line-port policy: 0 full, 1 read-only, 2 off |
/// | 7 | 1 | setup complete: 0 or 1 |
/// | 8 | 32 | hostname |
/// | 40 | 6 | reserved, zero |
/// | 46 | 2 | CRC-16 of bytes 0..46 |
#[must_use]
pub fn encode_device(device: &Device) -> [u8; DEVICE_RECORD_LEN] {
    let mut raw = [0u8; DEVICE_RECORD_LEN];
    open(&mut raw, DEVICE_MAGIC);
    raw[5] = device.hostname_len;
    raw[6] = device.ports.code();
    raw[7] = u8::from(device.setup_complete);
    raw[8..8 + MAX_HOSTNAME].copy_from_slice(&device.hostname);
    seal(&mut raw);
    raw
}

/// Parse the device record. `None` for erased flash and for anything that is
/// not a valid, current record - including a hostname a DHCP server would
/// refuse and a policy code this firmware does not know.
#[must_use]
pub fn decode_device(raw: &[u8]) -> Option<Device> {
    if !framed(raw, DEVICE_MAGIC, DEVICE_RECORD_LEN) {
        return None;
    }
    let hostname = text(raw, 8, usize::from(raw[5]), MAX_HOSTNAME)?;
    let ports = LinePolicy::from_code(raw[6])?;
    let setup_complete = match raw[7] {
        0 => false,
        1 => true,
        _ => return None,
    };
    Device::new(hostname, ports, setup_complete).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        check_hostname, decode_admin, decode_device, decode_wifi, encode_admin, encode_device,
        encode_wifi, parse_wifi_set, Admin, Device, LinePolicy, Wifi, ADMIN_RECORD_LEN,
        DEVICE_RECORD_LEN, HASH_LEN, MAX_HOSTNAME, MAX_SSID, MAX_WIFI_PASS, SALT_LEN,
        WIFI_RECORD_LEN,
    };

    fn wifi() -> Wifi {
        Wifi::new("bench", "secret").unwrap()
    }

    fn admin() -> Admin {
        Admin::new([0x5a; SALT_LEN], 10_000, [0xa5; HASH_LEN]).unwrap()
    }

    fn device() -> Device {
        Device::new("wfi-controller", LinePolicy::ReadOnly, true).unwrap()
    }

    #[test]
    fn the_wifi_record_round_trips() {
        let raw = encode_wifi(&wifi());
        assert_eq!(raw.len(), WIFI_RECORD_LEN);
        assert_eq!(decode_wifi(&raw), Some(wifi()));

        // The longest credentials still fit, and so does an open network.
        let ssid = core::str::from_utf8(&[b's'; MAX_SSID]).unwrap();
        let pass = core::str::from_utf8(&[b'p'; MAX_WIFI_PASS]).unwrap();
        let full = Wifi::new(ssid, pass).unwrap();
        assert_eq!(decode_wifi(&encode_wifi(&full)), Some(full));
        let open = Wifi::new("guest", "").unwrap();
        assert!(!open.has_pass());
        assert_eq!(decode_wifi(&encode_wifi(&open)), Some(open));
    }

    #[test]
    fn the_wifi_record_refuses_what_it_cannot_hold() {
        assert!(Wifi::new("", "secret").is_err());
        let long = core::str::from_utf8(&[b's'; MAX_SSID + 1]).unwrap();
        assert!(Wifi::new(long, "").is_err());
        let long = core::str::from_utf8(&[b'p'; MAX_WIFI_PASS + 1]).unwrap();
        assert!(Wifi::new("bench", long).is_err());
        assert!(Wifi::new("bench\n", "").is_err());
        assert!(Wifi::new("bench", "pass\tword").is_err());
    }

    #[test]
    fn a_damaged_wifi_record_reads_as_nothing() {
        assert_eq!(decode_wifi(&[0xff; WIFI_RECORD_LEN]), None);
        assert_eq!(decode_wifi(&[0x00; WIFI_RECORD_LEN]), None);
        assert_eq!(decode_wifi(&[]), None);

        let good = encode_wifi(&wifi());
        assert_eq!(decode_wifi(&good[..WIFI_RECORD_LEN - 1]), None);
        // A flipped bit in the SSID.
        let mut raw = good;
        raw[10] ^= 0x01;
        assert_eq!(decode_wifi(&raw), None);
        // A foreign magic.
        let mut raw = good;
        raw[0] = b'X';
        assert_eq!(decode_wifi(&raw), None);
        // A version from the future is not guessed at.
        let mut raw = good;
        raw[4] = 2;
        assert_eq!(decode_wifi(&raw), None);
        // A length field that does not fit its field, CRC and all.
        let mut raw = good;
        raw[6] = MAX_SSID as u8 + 1;
        super::seal(&mut raw);
        assert_eq!(decode_wifi(&raw), None);
    }

    #[test]
    fn the_password_is_not_in_the_debug_output() {
        let mut shown = std::string::String::new();
        core::fmt::write(&mut shown, format_args!("{:?}", wifi())).unwrap();
        assert!(shown.contains("bench"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn the_admin_record_round_trips() {
        let raw = encode_admin(&admin());
        assert_eq!(raw.len(), ADMIN_RECORD_LEN);
        let back = decode_admin(&raw).unwrap();
        assert_eq!(back.salt(), &[0x5a; SALT_LEN]);
        assert_eq!(back.iterations(), 10_000);
        assert_eq!(back.hash(), &[0xa5; HASH_LEN]);

        assert!(Admin::new([0; SALT_LEN], 0, [0; HASH_LEN]).is_err());
    }

    #[test]
    fn a_damaged_admin_record_reads_as_no_password() {
        assert_eq!(decode_admin(&[0xff; ADMIN_RECORD_LEN]), None);
        assert_eq!(decode_admin(&[0x00; ADMIN_RECORD_LEN]), None);
        assert_eq!(decode_admin(&[]), None);

        let good = encode_admin(&admin());
        assert_eq!(decode_admin(&good[..ADMIN_RECORD_LEN - 1]), None);
        let mut raw = good;
        raw[30] ^= 0x80;
        assert_eq!(decode_admin(&raw), None);
        let mut raw = good;
        raw[1] = b'X';
        assert_eq!(decode_admin(&raw), None);
        let mut raw = good;
        raw[4] = 7;
        assert_eq!(decode_admin(&raw), None);
        // Zero iterations, CRC'd: still not a password.
        let mut raw = good;
        raw[8..12].copy_from_slice(&0u32.to_le_bytes());
        super::seal(&mut raw);
        assert_eq!(decode_admin(&raw), None);
    }

    #[test]
    fn the_device_record_round_trips() {
        let raw = encode_device(&device());
        assert_eq!(raw.len(), DEVICE_RECORD_LEN);
        assert_eq!(decode_device(&raw), Some(device()));

        for ports in [LinePolicy::Full, LinePolicy::ReadOnly, LinePolicy::Off] {
            for complete in [false, true] {
                let d = Device::new("wfi028t-1a2b", ports, complete).unwrap();
                let back = decode_device(&encode_device(&d)).unwrap();
                assert_eq!(back.ports(), ports);
                assert_eq!(back.setup_complete(), complete);
                assert_eq!(back.hostname(), "wfi028t-1a2b");
            }
        }

        let longest = core::str::from_utf8(&[b'h'; MAX_HOSTNAME]).unwrap();
        let d = Device::new(longest, LinePolicy::Full, false).unwrap();
        assert_eq!(decode_device(&encode_device(&d)), Some(d));
        assert_eq!(d.with_hostname("renamed").unwrap().hostname(), "renamed");
    }

    #[test]
    fn a_damaged_device_record_reads_as_nothing() {
        assert_eq!(decode_device(&[0xff; DEVICE_RECORD_LEN]), None);
        assert_eq!(decode_device(&[0x00; DEVICE_RECORD_LEN]), None);
        assert_eq!(decode_device(&[]), None);

        let good = encode_device(&device());
        assert_eq!(decode_device(&good[..DEVICE_RECORD_LEN - 1]), None);
        let mut raw = good;
        raw[9] ^= 0x01;
        assert_eq!(decode_device(&raw), None);
        let mut raw = good;
        raw[2] = b'X';
        assert_eq!(decode_device(&raw), None);
        let mut raw = good;
        raw[4] = 0;
        assert_eq!(decode_device(&raw), None);
        // A policy code and a flag value from a future version.
        let mut raw = good;
        raw[6] = 9;
        super::seal(&mut raw);
        assert_eq!(decode_device(&raw), None);
        let mut raw = good;
        raw[7] = 2;
        super::seal(&mut raw);
        assert_eq!(decode_device(&raw), None);
        // A hostname a DHCP server would refuse.
        let mut raw = good;
        raw[8] = b'-';
        super::seal(&mut raw);
        assert_eq!(decode_device(&raw), None);
    }

    #[test]
    fn hostnames_are_checked_against_dhcp_and_mdns() {
        for good in ["wfi-controller", "wfi028t-1a2b", "a", "0", "a-b-c"] {
            assert!(check_hostname(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-wfi",
            "wfi-",
            "WFI",
            "wfi_controller",
            "wfi.local",
            "wfi controller",
            "wfi028t-\u{e9}",
        ] {
            assert!(check_hostname(bad).is_err(), "{bad}");
        }
        let long = core::str::from_utf8(&[b'h'; MAX_HOSTNAME + 1]).unwrap();
        assert!(check_hostname(long).is_err());
    }

    #[test]
    fn line_policies_round_trip_through_text_and_code() {
        for policy in [LinePolicy::Full, LinePolicy::ReadOnly, LinePolicy::Off] {
            assert_eq!(LinePolicy::from_code(policy.code()), Some(policy));
            assert_eq!(LinePolicy::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(LinePolicy::parse("readonly"), Some(LinePolicy::ReadOnly));
        assert_eq!(LinePolicy::parse("READ-ONLY"), Some(LinePolicy::ReadOnly));
        assert_eq!(LinePolicy::parse("nonsense"), None);
        assert_eq!(LinePolicy::from_code(3), None);
    }

    #[test]
    fn wifi_set_arguments_are_split_on_quotes() {
        assert_eq!(parse_wifi_set("bench secret"), Ok(("bench", "secret")));
        assert_eq!(
            parse_wifi_set("\"my network\" \"pass phrase\""),
            Ok(("my network", "pass phrase"))
        );
        assert_eq!(parse_wifi_set("bench \"\""), Ok(("bench", "")));
        assert_eq!(parse_wifi_set("\"bench\" secret"), Ok(("bench", "secret")));
        // Extra spaces between the fields are not part of either.
        assert_eq!(parse_wifi_set("bench   secret"), Ok(("bench", "secret")));

        assert!(parse_wifi_set("").is_err());
        assert!(parse_wifi_set("bench").is_err());
        assert!(parse_wifi_set("\"unterminated secret").is_err());
        assert!(parse_wifi_set("bench secret extra").is_err());
        assert!(parse_wifi_set("\"a\" \"b\" c").is_err());
    }
}
