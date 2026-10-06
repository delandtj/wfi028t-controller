//! The settings block, 0x003f-0x0081 (67 registers, read/write).
//!
//! Of the 67 registers, eight bits and words are confirmed on the bus
//! (`docs/register-map.md` "Confirmed"); the rest are factory parameters we
//! do not understand. The stock controller rewrites the whole block on every
//! user action, so [`Settings`] keeps the raw block and
//! [`Settings::apply`] returns a copy with exactly one bit or word changed.
//! Everything else - unknown registers, and the other bits of 0x003f - is
//! copied through byte for byte, which is what makes the whole-block write
//! safe.
//!
//! Setpoints here are whole degrees, unlike the sensors in
//! [`crate::status`]. Ranges are the manual's (P01 8-40, P02 8-28, P03 8-40,
//! P04 1-18) and are enforced in [`Settings::apply`], never on read: a block
//! read from the heat pump is reported as it is, even if a value is outside
//! what the manual allows.

use crate::rtu::{self, SETTINGS_LEN, SETTINGS_START, SETTINGS_WRITE_LEN};

/// Main bit-field register: power, P05 and ECO/boost.
pub const REG_FLAGS: u16 = 0x003f;
/// Operating mode.
pub const REG_MODE: u16 = 0x0040;
/// P01 heating setpoint.
pub const REG_HEAT_SETPOINT: u16 = 0x0041;
/// P02 cooling setpoint.
pub const REG_COOL_SETPOINT: u16 = 0x0042;
/// P03 auto setpoint.
pub const REG_AUTO_SETPOINT: u16 = 0x004a;
/// P04 restart hysteresis.
pub const REG_HYSTERESIS: u16 = 0x004d;

/// 0x003f bit 0: 1 = unit on.
pub const BIT_POWER: u8 = 0;
/// 0x003f bit 4: P05, 1 = stop once the target is reached.
pub const BIT_STOP_AT_TARGET: u8 = 4;
/// 0x003f bit 6: 1 = normal/ECO, 0 = full power. Inverted, see
/// [`Settings::boost`].
pub const BIT_ECO: u8 = 6;

/// Operating mode, 0x0040.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 1, confirmed on this unit.
    Heat,
    /// 2, from the vendor protocol.
    Cool,
    /// 7, from the vendor protocol.
    Auto,
    /// Anything else, preserved so a readback never loses information.
    Other(u16),
}

impl Mode {
    /// Decode the raw 0x0040 word.
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        match raw {
            1 => Self::Heat,
            2 => Self::Cool,
            7 => Self::Auto,
            other => Self::Other(other),
        }
    }

    /// The raw 0x0040 word.
    #[must_use]
    pub const fn to_raw(self) -> u16 {
        match self {
            Self::Heat => 1,
            Self::Cool => 2,
            Self::Auto => 7,
            Self::Other(raw) => raw,
        }
    }

    /// `true` for the three modes the device is allowed to write.
    #[must_use]
    pub const fn is_settable(self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

/// A range-checked numeric setting, named for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// P01, 0x0041.
    HeatSetpoint,
    /// P02, 0x0042.
    CoolSetpoint,
    /// P03, 0x004a.
    AutoSetpoint,
    /// P04, 0x004d.
    Hysteresis,
}

impl Field {
    /// Register holding the field.
    #[must_use]
    pub const fn register(self) -> u16 {
        match self {
            Self::HeatSetpoint => REG_HEAT_SETPOINT,
            Self::CoolSetpoint => REG_COOL_SETPOINT,
            Self::AutoSetpoint => REG_AUTO_SETPOINT,
            Self::Hysteresis => REG_HYSTERESIS,
        }
    }

    /// Inclusive range allowed by the manual.
    #[must_use]
    pub const fn limits(self) -> (u8, u8) {
        match self {
            Self::HeatSetpoint | Self::AutoSetpoint => (8, 40),
            Self::CoolSetpoint => (8, 28),
            Self::Hysteresis => (1, 18),
        }
    }
}

/// One validated change requested from outside (MQTT, bench, test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 0x003f bit 0.
    SetPower(bool),
    /// 0x003f bit 6, inverted: `true` = full power.
    SetBoost(bool),
    /// 0x003f bit 4: P05, `true` = stop once the target is reached.
    SetStopAtTarget(bool),
    /// 0x0040. [`Mode::Other`] is rejected.
    SetMode(Mode),
    /// P01, 8-40 C.
    SetHeatSetpoint(u8),
    /// P02, 8-28 C.
    SetCoolSetpoint(u8),
    /// P03, 8-40 C.
    SetAutoSetpoint(u8),
    /// P04, 1-18 C.
    SetHysteresis(u8),
}

/// Why a [`Command`] did not produce a new block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rejected {
    /// Value outside the manual's inclusive range for that field.
    OutOfRange {
        /// Field that was addressed.
        field: Field,
        /// Value that was asked for.
        value: u8,
        /// Lowest accepted value.
        min: u8,
        /// Highest accepted value.
        max: u8,
    },
    /// A mode we never confirmed on this unit; only heat, cool and auto are
    /// writable.
    UnsupportedMode(u16),
}

/// Decoded view of the settings block, and the source of every write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    raw: [u16; SETTINGS_LEN],
}

impl Settings {
    /// Wrap a complete settings block.
    #[must_use]
    pub const fn from_block(raw: [u16; SETTINGS_LEN]) -> Self {
        Self { raw }
    }

    /// Wrap a slice of exactly [`SETTINGS_LEN`] words.
    #[must_use]
    pub fn from_words(words: &[u16]) -> Option<Self> {
        let raw: [u16; SETTINGS_LEN] = words.try_into().ok()?;
        Some(Self { raw })
    }

    /// Decode the heat pump's reply to [`rtu::read_settings_request`].
    ///
    /// # Errors
    ///
    /// As [`rtu::parse_read_response`]: a malformed, foreign or exception
    /// frame, or a reply that does not carry 67 registers.
    pub fn from_response(frame: &[u8]) -> Result<Self, rtu::Error> {
        Ok(Self {
            raw: rtu::parse_read_block::<SETTINGS_LEN>(frame)?,
        })
    }

    /// The block as received, or as it will be written.
    #[must_use]
    pub const fn raw(&self) -> &[u16; SETTINGS_LEN] {
        &self.raw
    }

    /// Raw word at an absolute register address, or `None` outside the block.
    #[must_use]
    pub fn reg(&self, addr: u16) -> Option<u16> {
        let idx = addr.checked_sub(SETTINGS_START)?;
        self.raw.get(usize::from(idx)).copied()
    }

    /// The whole-block write frame for this block:
    /// `01 10 003f 0043 86 <134 bytes> <crc>`. The stock controller sends it
    /// three times, 500 ms apart.
    #[must_use]
    pub fn write_request(&self) -> [u8; SETTINGS_WRITE_LEN] {
        rtu::write_settings_request(&self.raw)
    }

    #[inline]
    fn at(&self, addr: u16) -> u16 {
        self.raw[(addr - SETTINGS_START) as usize]
    }

    #[inline]
    fn flag(&self, bit: u8) -> bool {
        self.at(REG_FLAGS) & (1 << bit) != 0
    }

    /// 0x003f bit 0: the unit is switched on.
    #[must_use]
    pub fn power(&self) -> bool {
        self.flag(BIT_POWER)
    }

    /// 0x003f bit 4: P05, stop once the target temperature is reached
    /// (`false` = non stop).
    #[must_use]
    pub fn stop_at_target(&self) -> bool {
        self.flag(BIT_STOP_AT_TARGET)
    }

    /// 0x003f bit 6, **inverted**: the bit is set for normal/ECO and clear
    /// for full power, so boost is on when the bit is clear. The vendor
    /// protocol calls this bit "silent mode" and claims boost lives in 0x0040
    /// bit 4; the capture disagrees.
    #[must_use]
    pub fn boost(&self) -> bool {
        !self.flag(BIT_ECO)
    }

    /// 0x0040 operating mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        Mode::from_raw(self.at(REG_MODE))
    }

    /// P01 heating setpoint, whole degrees as stored.
    #[must_use]
    pub fn heat_setpoint(&self) -> u16 {
        self.at(REG_HEAT_SETPOINT)
    }

    /// P02 cooling setpoint, whole degrees as stored.
    #[must_use]
    pub fn cool_setpoint(&self) -> u16 {
        self.at(REG_COOL_SETPOINT)
    }

    /// P03 auto setpoint, whole degrees as stored.
    #[must_use]
    pub fn auto_setpoint(&self) -> u16 {
        self.at(REG_AUTO_SETPOINT)
    }

    /// P04 restart hysteresis, whole degrees as stored.
    #[must_use]
    pub fn hysteresis(&self) -> u16 {
        self.at(REG_HYSTERESIS)
    }

    /// The setpoint that governs the current mode, if the mode is known.
    #[must_use]
    pub fn active_setpoint(&self) -> Option<u16> {
        match self.mode() {
            Mode::Heat => Some(self.heat_setpoint()),
            Mode::Cool => Some(self.cool_setpoint()),
            Mode::Auto => Some(self.auto_setpoint()),
            Mode::Other(_) => None,
        }
    }

    #[inline]
    fn with_word(&self, addr: u16, value: u16) -> Self {
        let mut next = *self;
        next.raw[(addr - SETTINGS_START) as usize] = value;
        next
    }

    #[inline]
    fn with_flag(&self, bit: u8, set: bool) -> Self {
        let mask = 1u16 << bit;
        let current = self.at(REG_FLAGS);
        let value = if set { current | mask } else { current & !mask };
        self.with_word(REG_FLAGS, value)
    }

    fn with_field(&self, field: Field, value: u8) -> Result<Self, Rejected> {
        let (min, max) = field.limits();
        if value < min || value > max {
            return Err(Rejected::OutOfRange {
                field,
                value,
                min,
                max,
            });
        }
        Ok(self.with_word(field.register(), u16::from(value)))
    }

    /// Apply one command, returning a new block in which only the targeted
    /// bit or word differs. Every other register, including the unknown ones
    /// and the other bits of 0x003f, is passed through untouched.
    ///
    /// # Errors
    ///
    /// [`Rejected::OutOfRange`] for a setpoint outside the manual's range,
    /// [`Rejected::UnsupportedMode`] for [`Mode::Other`].
    pub fn apply(&self, command: Command) -> Result<Self, Rejected> {
        match command {
            Command::SetPower(on) => Ok(self.with_flag(BIT_POWER, on)),
            // Inverted: the ECO bit is clear while full power runs.
            Command::SetBoost(on) => Ok(self.with_flag(BIT_ECO, !on)),
            Command::SetStopAtTarget(on) => Ok(self.with_flag(BIT_STOP_AT_TARGET, on)),
            Command::SetMode(mode) => {
                if mode.is_settable() {
                    Ok(self.with_word(REG_MODE, mode.to_raw()))
                } else {
                    Err(Rejected::UnsupportedMode(mode.to_raw()))
                }
            }
            Command::SetHeatSetpoint(c) => self.with_field(Field::HeatSetpoint, c),
            Command::SetCoolSetpoint(c) => self.with_field(Field::CoolSetpoint, c),
            Command::SetAutoSetpoint(c) => self.with_field(Field::AutoSetpoint, c),
            Command::SetHysteresis(c) => self.with_field(Field::Hysteresis, c),
        }
    }

    /// Registers that differ from `other`, as `(address, mine, theirs)`. For
    /// logging a write or a surprising readback.
    pub fn diff<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = (u16, u16, u16)> + 'a {
        self.raw
            .iter()
            .zip(other.raw.iter())
            .enumerate()
            .filter_map(|(i, (&mine, &theirs))| {
                (mine != theirs).then_some((SETTINGS_START + i as u16, mine, theirs))
            })
    }
}

#[cfg(test)]
pub(crate) mod snapshot {
    use super::SETTINGS_LEN;

    /// The settings block of `docs/register-map.md` "Snapshot: settings block
    /// 0x003f-0x0081", same window as the status snapshot: unit on, heating,
    /// ECO, P01 33, P02 27, P03 27, P04 1.
    pub const SETTINGS_BLOCK: [u16; SETTINGS_LEN] = [
        // 0x003f
        0x1071, 0x0001, 0x0021, 0x001b, 0x0032, 0x0096, 0xffff, 0xffff, // 0x0047
        0x7fff, 0xffff, 0x01f4, 0x001b, 0x000a, 0xffff, 0x0001, 0x0000, // 0x004f
        0xffec, 0x0028, 0xfffa, 0x000b, 0x0010, 0x0006, 0x0011, 0x001e, // 0x0057
        0x0001, 0x0058, 0x0028, 0x0008, 0x0001, 0x0017, 0x0028, 0x002c, // 0x005f
        0x0030, 0x0036, 0x003a, 0x0040, 0x0048, 0x0050, 0x0054, 0x005a, // 0x0067
        0x005f, 0x0064, 0x0069, 0x006e, 0x0073, 0x000c, 0x000d, 0x000e, // 0x006f
        0x002e, 0x0034, 0x003a, 0x0040, 0x0048, 0x0055, 0x0000, 0x0001, // 0x0077
        0x000c, 0xffff, 0x0000, 0x0008, 0x0000, 0x000c, 0x0000, 0x000e, // 0x007f
        0x0000, 0x0011, 0x0000,
    ];
}

#[cfg(test)]
mod tests {
    use super::{snapshot::SETTINGS_BLOCK, Command, Field, Mode, Rejected, Settings};
    use crate::rtu::{
        self, test_support, SETTINGS_LEN, SETTINGS_RESPONSE_LEN, SETTINGS_START, SETTINGS_WRITE_LEN,
    };

    fn snap() -> Settings {
        Settings::from_block(SETTINGS_BLOCK)
    }

    #[test]
    fn snapshot_decodes_to_the_logged_state() {
        let s = snap();
        // 0x003f = 0x1071: bit 0 on, bit 4 P05 set, bit 6 set -> ECO.
        assert!(s.power());
        assert!(s.stop_at_target());
        assert!(!s.boost());
        assert_eq!(s.mode(), Mode::Heat);
        assert_eq!(s.heat_setpoint(), 33);
        assert_eq!(s.cool_setpoint(), 27);
        assert_eq!(s.auto_setpoint(), 27);
        assert_eq!(s.hysteresis(), 1);
        assert_eq!(s.active_setpoint(), Some(33));
    }

    #[test]
    fn boost_is_the_inverted_eco_bit() {
        // The capture's other value of 0x003f: 4145 = 0x1031, bit 6 clear.
        let s = Settings::from_block(SETTINGS_BLOCK).with_flags(0x1031);
        assert!(s.boost());
        assert!(s.power());
        assert!(s.stop_at_target());

        // Toggling boost on clears bit 6 and touches nothing else.
        let on = snap().apply(Command::SetBoost(true)).unwrap();
        assert_eq!(on.reg(0x003f), Some(0x1031));
        assert!(on.boost());
        assert_eq!(on.raw()[1..], SETTINGS_BLOCK[1..]);

        // ...and back: bit 6 set again, byte-identical to the snapshot.
        let off = on.apply(Command::SetBoost(false)).unwrap();
        assert_eq!(off, snap());
    }

    #[test]
    fn power_and_p05_bits_follow_the_capture() {
        // 15:52:40 off: 0x1061 -> 0x1060 (bit 0 cleared, nothing else).
        let eco_boosting = snap().with_flags(0x1061);
        let off = eco_boosting.apply(Command::SetPower(false)).unwrap();
        assert_eq!(off.reg(0x003f), Some(0x1060));
        assert!(!off.power());
        let on = off.apply(Command::SetPower(true)).unwrap();
        assert_eq!(on, eco_boosting);

        let non_stop = snap().apply(Command::SetStopAtTarget(false)).unwrap();
        assert_eq!(non_stop.reg(0x003f), Some(0x1061));
        assert!(!non_stop.stop_at_target());
        assert!(non_stop.power());
        assert!(!non_stop.boost());
    }

    #[test]
    fn setpoint_changes_touch_one_register() {
        let s = snap();
        let hotter = s.apply(Command::SetHeatSetpoint(34)).unwrap();
        assert_eq!(hotter.heat_setpoint(), 34);
        let changed: Diffs = hotter.diff(&s).collect();
        assert_eq!(changed.as_slice(), &[(0x0041, 34, 33)]);

        for (cmd, addr, want) in [
            (Command::SetCoolSetpoint(28), 0x0042u16, 28u16),
            (Command::SetAutoSetpoint(28), 0x004a, 28),
            (Command::SetHysteresis(2), 0x004d, 2),
            (Command::SetMode(Mode::Cool), 0x0040, 2),
            (Command::SetMode(Mode::Auto), 0x0040, 7),
        ] {
            let next = s.apply(cmd).unwrap();
            assert_eq!(next.reg(addr), Some(want), "{cmd:?}");
            let diffs: Diffs = next.diff(&s).collect();
            assert_eq!(diffs.len(), 1, "{cmd:?} changed {diffs:?}");
            assert_eq!(diffs[0].0, addr);
        }
    }

    #[test]
    fn out_of_range_commands_are_rejected() {
        let s = snap();
        let cases = [
            (
                Command::SetHeatSetpoint(7),
                Field::HeatSetpoint,
                7u8,
                8u8,
                40u8,
            ),
            (Command::SetHeatSetpoint(41), Field::HeatSetpoint, 41, 8, 40),
            (Command::SetCoolSetpoint(29), Field::CoolSetpoint, 29, 8, 28),
            (Command::SetCoolSetpoint(0), Field::CoolSetpoint, 0, 8, 28),
            (Command::SetAutoSetpoint(41), Field::AutoSetpoint, 41, 8, 40),
            (Command::SetHysteresis(0), Field::Hysteresis, 0, 1, 18),
            (Command::SetHysteresis(19), Field::Hysteresis, 19, 1, 18),
        ];
        for (cmd, field, value, min, max) in cases {
            assert_eq!(
                s.apply(cmd),
                Err(Rejected::OutOfRange {
                    field,
                    value,
                    min,
                    max
                }),
                "{cmd:?}"
            );
        }
        // Edges are accepted.
        assert!(s.apply(Command::SetHeatSetpoint(8)).is_ok());
        assert!(s.apply(Command::SetHeatSetpoint(40)).is_ok());
        assert!(s.apply(Command::SetCoolSetpoint(28)).is_ok());
        assert!(s.apply(Command::SetHysteresis(1)).is_ok());
        assert!(s.apply(Command::SetHysteresis(18)).is_ok());
    }

    #[test]
    fn unknown_modes_are_preserved_on_read_and_refused_on_write() {
        let odd = snap().with_word_at(0x0040, 5);
        assert_eq!(odd.mode(), Mode::Other(5));
        assert_eq!(odd.active_setpoint(), None);
        assert_eq!(odd.reg(0x0040), Some(5));
        assert_eq!(
            snap().apply(Command::SetMode(Mode::Other(5))),
            Err(Rejected::UnsupportedMode(5))
        );
        assert_eq!(Mode::from_raw(1), Mode::Heat);
        assert_eq!(Mode::Heat.to_raw(), 1);
        assert_eq!(Mode::Other(9).to_raw(), 9);
        assert!(!Mode::Other(9).is_settable());
    }

    #[test]
    fn read_apply_write_preserves_every_untouched_byte() {
        // Full round trip: heat pump reply -> Settings -> apply -> 0x10 frame.
        let mut reply = [0u8; SETTINGS_RESPONSE_LEN];
        let n = test_support::read_response(&SETTINGS_BLOCK, &mut reply);
        assert_eq!(n, SETTINGS_RESPONSE_LEN);
        assert_eq!(reply[2], 134); // the capture's 134 data bytes

        let current = Settings::from_response(&reply).unwrap();
        assert_eq!(current, snap());

        let wanted = current.apply(Command::SetHeatSetpoint(34)).unwrap();
        let frame = wanted.write_request();
        assert_eq!(frame.len(), SETTINGS_WRITE_LEN);
        assert_eq!(&frame[..7], &[0x01, 0x10, 0x00, 0x3f, 0x00, 0x43, 0x86]);

        // Only the two bytes of 0x0041 differ from the data we read back.
        let read_data = &reply[3..3 + 2 * SETTINGS_LEN];
        let written = &frame[7..7 + 2 * SETTINGS_LEN];
        let differing: ByteIndices = written
            .iter()
            .zip(read_data.iter())
            .enumerate()
            .filter_map(|(i, (a, b))| (a != b).then_some(i))
            .collect();
        let idx = 2 * usize::from(0x0041 - SETTINGS_START);
        assert_eq!(differing.as_slice(), &[idx + 1]);
        assert_eq!(written[idx], 0x00);
        assert_eq!(written[idx + 1], 34);

        // The ack the heat pump sends is accepted.
        let mut ack = [0x01u8, 0x10, 0x00, 0x3f, 0x00, 0x43, 0, 0];
        let crc = rtu::crc16(&ack[..6]);
        ack[6] = crc as u8;
        ack[7] = (crc >> 8) as u8;
        assert_eq!(rtu::parse_settings_write_ack(&ack), Ok(()));

        // And a readback of what we wrote decodes to what we asked for.
        let mut readback = [0u8; SETTINGS_RESPONSE_LEN];
        test_support::read_response(wanted.raw(), &mut readback);
        assert_eq!(Settings::from_response(&readback).unwrap(), wanted);
    }

    #[test]
    fn unknown_registers_survive_a_chain_of_commands() {
        let s = snap();
        let after = s
            .apply(Command::SetPower(false))
            .and_then(|s| s.apply(Command::SetMode(Mode::Cool)))
            .and_then(|s| s.apply(Command::SetCoolSetpoint(24)))
            .and_then(|s| s.apply(Command::SetBoost(true)))
            .and_then(|s| s.apply(Command::SetStopAtTarget(false)))
            .unwrap();
        let diffs: Diffs = after.diff(&s).collect();
        assert_eq!(
            diffs.as_slice(),
            &[(0x003f, 0x1020, 0x1071), (0x0040, 2, 1), (0x0042, 24, 27)]
        );
        // Every register outside 0x003f, 0x0040 and 0x0042 is untouched.
        for (i, (&a, &b)) in after.raw().iter().zip(SETTINGS_BLOCK.iter()).enumerate() {
            let addr = SETTINGS_START + i as u16;
            if !matches!(addr, 0x003f | 0x0040 | 0x0042) {
                assert_eq!(a, b, "{addr:#06x}");
            }
        }
    }

    #[test]
    fn block_constructors_agree_and_reject_wrong_lengths() {
        assert_eq!(Settings::from_words(&SETTINGS_BLOCK), Some(snap()));
        assert!(Settings::from_words(&SETTINGS_BLOCK[..66]).is_none());
        assert_eq!(snap().reg(0x0081), Some(0x0000));
        assert_eq!(snap().reg(0x0082), None);
        assert_eq!(snap().reg(0x003e), None);

        let mut reply = [0u8; SETTINGS_RESPONSE_LEN];
        let n = test_support::read_response(&SETTINGS_BLOCK[..66], &mut reply);
        assert!(matches!(
            Settings::from_response(&reply[..n]),
            Err(rtu::Error::BadByteCount(132))
        ));
    }

    // --- small test-only helpers ---

    type Diffs = std::vec::Vec<(u16, u16, u16)>;
    type ByteIndices = std::vec::Vec<usize>;

    impl Settings {
        fn with_flags(self, value: u16) -> Self {
            self.with_word_at(0x003f, value)
        }

        fn with_word_at(self, addr: u16, value: u16) -> Self {
            let mut raw = *self.raw();
            raw[usize::from(addr - SETTINGS_START)] = value;
            Settings::from_block(raw)
        }
    }
}
