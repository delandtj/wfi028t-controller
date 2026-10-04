//! The OTA push header: layout, parsing, and the signature check. Pure: no
//! I/O, no clock, no allocation, nothing hardware-bound.
//!
//! This file is the single source of truth for the wire format and is
//! compiled into both ends: the firmware's receiver ([`super`]) and the host
//! tool, which includes it by path
//! (`tools/fw-ota/src/main.rs`, `#[path = ...] mod header;`). That is also
//! how its tests run on the host - `cargo test -p fw-ota` - the way
//! `fw/src/mqtt/entity.rs` is tested, but with the harness committed instead
//! of thrown away. Nothing here may refer to `crate::`.
//!
//! # Layout (little endian)
//!
//! ```text
//! offset  size  field
//!      0     4  magic       "WOTA"
//!      4     1  version     1
//!      5     3  reserved    zero
//!      8     4  image_len   u32, bytes of image following the header
//!     12    32  image_sha   SHA-256 of those bytes
//!     44    16  target      "wfi028t-c6", NUL padded; refused if not ours
//!     60    16  fw_version  "0.1.0", NUL padded; informational, logged
//!     76    64  signature   ed25519 over bytes 0..76 of this header
//!    140        end of header; image_len bytes of image follow
//! ```
//!
//! The signature covers the header only, never the image: the image is bound
//! to it by `image_sha`, which is checked as the bytes are written. An
//! attacker can therefore not substitute an image without breaking the SHA,
//! and the device never has to buffer a megabyte to verify one signature.
//!
//! ## Deviation from ADR 0002
//!
//! The ADR's wire-format block lists the field widths above (they sum to 76
//! bytes) but calls the header 128 bytes and the signed region bytes 0..64;
//! 76 + 64 is 140, so the three numbers cannot all hold. This implementation
//! keeps every field at its stated width, offset and order, puts the
//! signature last, and signs everything before it - so [`HEADER_LEN`] is 140
//! and [`SIGNED_LEN`] is 76. The alternative reading (a 128-byte header with
//! `target` cut to 12 bytes and `fw_version` to 8) is one edit away here and
//! in no other file, and no signed image exists yet. Flagged for the ADR.
//!
//! A reader that does not recognise [`VERSION`] rejects the push, which is
//! what lets a future version change everything after byte 5.

/// Header magic, first four bytes on the wire.
pub const MAGIC: [u8; 4] = *b"WOTA";

/// Wire-format version this code speaks. Anything else is refused rather
/// than guessed at.
pub const VERSION: u8 = 1;

/// Bytes covered by the signature: the whole header except the signature.
pub const SIGNED_LEN: usize = 76;

/// An ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

/// Total header length, signature included.
pub const HEADER_LEN: usize = SIGNED_LEN + SIGNATURE_LEN;

/// Width of the `target` field.
pub const TARGET_LEN: usize = 16;

/// Width of the `fw_version` field.
pub const FW_VERSION_LEN: usize = 16;

/// An ed25519 public key, as committed in `fw/ota-signing.pub`.
pub const PUBLIC_KEY_LEN: usize = 32;

/// A SHA-256 digest.
pub const SHA_LEN: usize = 32;

/// The only target this firmware accepts an image for. An image built for
/// another board (or another chip) is refused before anything is erased.
pub const TARGET: &str = "wfi028t-c6";

// Field offsets, used by both ends and by the tests.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_RESERVED: usize = 5;
const OFF_IMAGE_LEN: usize = 8;
const OFF_IMAGE_SHA: usize = 12;
const OFF_TARGET: usize = 44;
const OFF_FW_VERSION: usize = 60;
const OFF_SIGNATURE: usize = SIGNED_LEN;

/// Why a push was refused. The [`Reject::as_str`] text is what goes back to
/// the host as `err <reason>`, so it is protocol-visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// Fewer than [`HEADER_LEN`] bytes of header arrived.
    Short,
    /// The first four bytes are not [`MAGIC`].
    Magic,
    /// A header version this firmware does not speak.
    Version(u8),
    /// The reserved bytes are not zero.
    Reserved,
    /// The image was built for another target.
    Target,
    /// The signature does not check out against the compiled-in public key.
    Signature,
    /// `image_len` is zero.
    Empty,
    /// `image_len` does not fit the app slot.
    TooBig,
    /// A field handed to [`Header::new`] does not fit its fixed width.
    ///
    /// Only the sending side can produce this; the firmware never does, which
    /// is why it is allowed to look dead there.
    #[allow(dead_code)]
    FieldTooLong,
}

impl Reject {
    /// Protocol-visible reason text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short header",
            Self::Magic => "bad magic (not an image for this tool)",
            Self::Version(_) => "unsupported header version",
            Self::Reserved => "reserved bytes are not zero",
            Self::Target => "image is for another target",
            Self::Signature => "bad signature",
            Self::Empty => "empty image",
            Self::TooBig => "image does not fit the app slot",
            Self::FieldTooLong => "field too long",
        }
    }
}

/// One parsed push header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Bytes of image that follow the header.
    pub image_len: u32,
    /// SHA-256 of those bytes.
    pub image_sha: [u8; SHA_LEN],
    /// Target name, NUL padded.
    pub target: [u8; TARGET_LEN],
    /// Firmware version string, NUL padded. Informational.
    pub fw_version: [u8; FW_VERSION_LEN],
    /// ed25519 signature over the first [`SIGNED_LEN`] bytes.
    pub signature: [u8; SIGNATURE_LEN],
}

impl Header {
    /// Build an unsigned header. Fill [`Header::signature`] with a signature
    /// over [`Header::signed_bytes`] afterwards.
    ///
    /// # Errors
    /// [`Reject::FieldTooLong`] if `target` or `fw_version` does not fit its
    /// field with room for the NUL terminator.
    // The sending side's half of this file (`new`, `encode`, `fixed`): the
    // firmware only ever parses, so these are dead in that build on purpose.
    #[allow(dead_code)]
    pub fn new(
        image_len: u32,
        image_sha: [u8; SHA_LEN],
        target: &str,
        fw_version: &str,
    ) -> Result<Self, Reject> {
        Ok(Self {
            image_len,
            image_sha,
            target: fixed::<TARGET_LEN>(target)?,
            fw_version: fixed::<FW_VERSION_LEN>(fw_version)?,
            signature: [0u8; SIGNATURE_LEN],
        })
    }

    /// The header as it goes on the wire.
    #[must_use]
    #[allow(dead_code)] // sending side; see `new`
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut raw = [0u8; HEADER_LEN];
        raw[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC);
        raw[OFF_VERSION] = VERSION;
        // OFF_RESERVED stays zero.
        raw[OFF_IMAGE_LEN..OFF_IMAGE_LEN + 4].copy_from_slice(&self.image_len.to_le_bytes());
        raw[OFF_IMAGE_SHA..OFF_IMAGE_SHA + SHA_LEN].copy_from_slice(&self.image_sha);
        raw[OFF_TARGET..OFF_TARGET + TARGET_LEN].copy_from_slice(&self.target);
        raw[OFF_FW_VERSION..OFF_FW_VERSION + FW_VERSION_LEN].copy_from_slice(&self.fw_version);
        raw[OFF_SIGNATURE..].copy_from_slice(&self.signature);
        raw
    }

    /// The bytes a signature covers: the header without its signature.
    ///
    /// # Errors
    /// [`Reject::Short`] if `raw` is not a whole header.
    pub fn signed_bytes(raw: &[u8]) -> Result<&[u8], Reject> {
        if raw.len() < HEADER_LEN {
            return Err(Reject::Short);
        }
        Ok(&raw[..SIGNED_LEN])
    }

    /// Parse a header. Checks the framing only (length, magic, version,
    /// reserved bytes); the signature and the target are
    /// [`Header::accept`]'s job.
    ///
    /// # Errors
    /// [`Reject::Short`], [`Reject::Magic`], [`Reject::Version`] or
    /// [`Reject::Reserved`].
    pub fn parse(raw: &[u8]) -> Result<Self, Reject> {
        if raw.len() < HEADER_LEN {
            return Err(Reject::Short);
        }
        if raw[OFF_MAGIC..OFF_MAGIC + 4] != MAGIC {
            return Err(Reject::Magic);
        }
        if raw[OFF_VERSION] != VERSION {
            return Err(Reject::Version(raw[OFF_VERSION]));
        }
        if raw[OFF_RESERVED..OFF_RESERVED + 3] != [0u8; 3] {
            return Err(Reject::Reserved);
        }

        let mut image_len = [0u8; 4];
        image_len.copy_from_slice(&raw[OFF_IMAGE_LEN..OFF_IMAGE_LEN + 4]);
        let mut image_sha = [0u8; SHA_LEN];
        image_sha.copy_from_slice(&raw[OFF_IMAGE_SHA..OFF_IMAGE_SHA + SHA_LEN]);
        let mut target = [0u8; TARGET_LEN];
        target.copy_from_slice(&raw[OFF_TARGET..OFF_TARGET + TARGET_LEN]);
        let mut fw_version = [0u8; FW_VERSION_LEN];
        fw_version.copy_from_slice(&raw[OFF_FW_VERSION..OFF_FW_VERSION + FW_VERSION_LEN]);
        let mut signature = [0u8; SIGNATURE_LEN];
        signature.copy_from_slice(&raw[OFF_SIGNATURE..HEADER_LEN]);

        Ok(Self {
            image_len: u32::from_le_bytes(image_len),
            image_sha,
            target,
            fw_version,
            signature,
        })
    }

    /// Parse and check a header in the order ADR 0002 requires: framing,
    /// target, signature, then size against the slot. Nothing is erased
    /// before this returns `Ok`.
    ///
    /// # Errors
    /// Any [`Reject`]; the first failure wins, so an unsigned push is never
    /// told whether its length would have fitted.
    pub fn accept(
        raw: &[u8],
        expect_target: &str,
        public_key: &[u8; PUBLIC_KEY_LEN],
        slot_len: u32,
    ) -> Result<Self, Reject> {
        let header = Self::parse(raw)?;
        if header.target_name() != expect_target {
            return Err(Reject::Target);
        }
        if !verify(Self::signed_bytes(raw)?, &header.signature, public_key) {
            return Err(Reject::Signature);
        }
        if header.image_len == 0 {
            return Err(Reject::Empty);
        }
        if header.image_len > slot_len {
            return Err(Reject::TooBig);
        }
        Ok(header)
    }

    /// The target field as text, up to the first NUL. Empty if it is not
    /// UTF-8, which can then never match a real target name.
    #[must_use]
    pub fn target_name(&self) -> &str {
        text(&self.target)
    }

    /// The version field as text, up to the first NUL.
    #[must_use]
    pub fn fw_version_name(&self) -> &str {
        text(&self.fw_version)
    }
}

/// Is `signature` a valid ed25519 signature over `signed` by `public_key`?
///
/// This is the device-side verifier (`ed25519-compact`, no_std, no
/// allocation), so the host tests exercise exactly the code that runs on the
/// board.
#[must_use]
pub fn verify(
    signed: &[u8],
    signature: &[u8; SIGNATURE_LEN],
    public_key: &[u8; PUBLIC_KEY_LEN],
) -> bool {
    let Ok(key) = ed25519_compact::PublicKey::from_slice(public_key) else {
        return false;
    };
    let signature = ed25519_compact::Signature::new(*signature);
    key.verify(signed, &signature).is_ok()
}

/// Copy `text` into a fixed-width NUL-padded field.
#[allow(dead_code)] // sending side; see `Header::new`
fn fixed<const N: usize>(text: &str) -> Result<[u8; N], Reject> {
    let bytes = text.as_bytes();
    // Strictly shorter than the field: a name that filled it exactly would
    // have no NUL terminator, and nothing needs the last byte.
    if bytes.len() >= N {
        return Err(Reject::FieldTooLong);
    }
    let mut field = [0u8; N];
    field[..bytes.len()].copy_from_slice(bytes);
    Ok(field)
}

/// A NUL-padded field as text. `""` for anything that is not UTF-8.
fn text(field: &[u8]) -> &str {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..end]).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Host tests (run from the fw-ota crate: `cargo test -p fw-ota`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        Header, Reject, FW_VERSION_LEN, HEADER_LEN, MAGIC, SHA_LEN, SIGNED_LEN, TARGET, TARGET_LEN,
        VERSION,
    };

    const SLOT: u32 = 0x40_0000;

    /// A deterministic key pair, so a failing test is reproducible.
    fn key_pair() -> ed25519_compact::KeyPair {
        let seed = ed25519_compact::Seed::new([7u8; 32]);
        ed25519_compact::KeyPair::from_seed(seed)
    }

    fn public_key() -> [u8; 32] {
        *key_pair().pk
    }

    /// A signed header for an image of `image_len` bytes, hash `0xab`*32.
    fn signed(image_len: u32, target: &str, fw_version: &str) -> [u8; HEADER_LEN] {
        let mut header = Header::new(image_len, [0xab; SHA_LEN], target, fw_version).unwrap();
        let raw = header.encode();
        let signature = key_pair().sk.sign(&raw[..SIGNED_LEN], None);
        header.signature = *signature;
        header.encode()
    }

    #[test]
    fn layout_is_the_documented_one() {
        assert_eq!(HEADER_LEN, 140);
        assert_eq!(SIGNED_LEN, 76);
        let raw = signed(1024, TARGET, "0.1.0");
        assert_eq!(&raw[0..4], &MAGIC);
        assert_eq!(raw[4], VERSION);
        assert_eq!(&raw[5..8], &[0, 0, 0]);
        assert_eq!(u32::from_le_bytes(raw[8..12].try_into().unwrap()), 1024);
        assert_eq!(&raw[12..44], &[0xab; 32]);
        assert_eq!(&raw[44..54], TARGET.as_bytes());
        assert_eq!(&raw[54..60], &[0; 6]);
        assert_eq!(&raw[60..65], b"0.1.0");
    }

    #[test]
    fn round_trip() {
        let raw = signed(755_000, TARGET, "0.2.0");
        let header = Header::parse(&raw).unwrap();
        assert_eq!(header.image_len, 755_000);
        assert_eq!(header.image_sha, [0xab; SHA_LEN]);
        assert_eq!(header.target_name(), TARGET);
        assert_eq!(header.fw_version_name(), "0.2.0");
        assert_eq!(header.encode(), raw);
    }

    #[test]
    fn a_good_push_is_accepted() {
        let raw = signed(755_000, TARGET, "0.2.0");
        let header = Header::accept(&raw, TARGET, &public_key(), SLOT).unwrap();
        assert_eq!(header.image_len, 755_000);
    }

    #[test]
    fn framing_is_checked() {
        let good = signed(1024, TARGET, "0.2.0");

        assert_eq!(Header::parse(&good[..HEADER_LEN - 1]), Err(Reject::Short));

        let mut raw = good;
        raw[0] = b'X';
        assert_eq!(Header::parse(&raw), Err(Reject::Magic));

        let mut raw = good;
        raw[4] = 2;
        assert_eq!(Header::parse(&raw), Err(Reject::Version(2)));

        let mut raw = good;
        raw[6] = 1;
        assert_eq!(Header::parse(&raw), Err(Reject::Reserved));
    }

    #[test]
    fn another_target_is_refused_before_the_signature() {
        // Signed with our key, but for another board: still refused, and the
        // target check comes first so the reason says so.
        let raw = signed(1024, "sniffer-c6", "0.2.0");
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::Target)
        );
    }

    #[test]
    fn a_tampered_header_is_refused() {
        let good = signed(1024, TARGET, "0.2.0");

        // One bit of the image hash: the classic "swap the payload" attempt.
        let mut raw = good;
        raw[12] ^= 0x01;
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::Signature)
        );

        // The length field.
        let mut raw = good;
        raw[8] ^= 0x01;
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::Signature)
        );

        // The signature itself.
        let mut raw = good;
        raw[HEADER_LEN - 1] ^= 0x01;
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::Signature)
        );

        // An all-zero signature, as a header that was never signed carries.
        let mut raw = good;
        raw[SIGNED_LEN..].fill(0);
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::Signature)
        );
    }

    #[test]
    fn another_key_is_refused() {
        let raw = signed(1024, TARGET, "0.2.0");
        let other = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new([9u8; 32]));
        assert_eq!(
            Header::accept(&raw, TARGET, &other.pk, SLOT),
            Err(Reject::Signature)
        );
    }

    #[test]
    fn the_image_has_to_fit_the_slot() {
        let raw = signed(0, TARGET, "0.2.0");
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::Empty)
        );

        let raw = signed(SLOT + 1, TARGET, "0.2.0");
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::TooBig)
        );

        // Exactly the slot size is fine.
        let raw = signed(SLOT, TARGET, "0.2.0");
        assert!(Header::accept(&raw, TARGET, &public_key(), SLOT).is_ok());
    }

    #[test]
    fn fields_have_to_fit_with_room_for_the_nul() {
        let long_target = "x".repeat(TARGET_LEN);
        assert_eq!(
            Header::new(1, [0; SHA_LEN], &long_target, "0.1.0"),
            Err(Reject::FieldTooLong)
        );
        let long_version = "y".repeat(FW_VERSION_LEN);
        assert_eq!(
            Header::new(1, [0; SHA_LEN], TARGET, &long_version),
            Err(Reject::FieldTooLong)
        );
        // One byte shorter is accepted and round-trips.
        let fits = "y".repeat(FW_VERSION_LEN - 1);
        let header = Header::new(1, [0; SHA_LEN], TARGET, &fits).unwrap();
        assert_eq!(header.fw_version_name(), fits);
    }

    #[test]
    fn a_field_that_is_not_utf8_never_matches() {
        let mut header = Header::new(1, [0; SHA_LEN], TARGET, "0.1.0").unwrap();
        header.target[0] = 0xff;
        assert_eq!(header.target_name(), "");
        let raw = header.encode();
        assert_eq!(
            Header::accept(&raw, TARGET, &public_key(), SLOT),
            Err(Reject::Target)
        );
    }

    #[test]
    fn every_reason_has_text() {
        for reject in [
            Reject::Short,
            Reject::Magic,
            Reject::Version(9),
            Reject::Reserved,
            Reject::Target,
            Reject::Signature,
            Reject::Empty,
            Reject::TooBig,
            Reject::FieldTooLong,
        ] {
            assert!(!reject.as_str().is_empty());
            // One line, so it can go out as `err <reason>`.
            assert!(!reject.as_str().contains('\n'));
        }
    }
}
