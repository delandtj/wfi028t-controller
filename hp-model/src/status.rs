//! The status block, 0x0000-0x003e (63 registers, read-only).
//!
//! Register meanings and scaling come from `docs/register-map.md`
//! ("Sensors", "Status and alarms"). Mapping A01-A14 was done by reading the
//! controller's parameter menu while the capture was running, so the scaling
//! is per register and not uniform: inlet water is tenths, most temperatures
//! are half degrees, exhaust is whole degrees, and the non-temperature values
//! (EEV steps, amps, volts, Hz, rpm) are raw.
//!
//! Temperatures are exposed as [`DeciCelsius`], a plain `i16` in tenths of a
//! degree. Fixed-point rather than `f32`: every raw scaling here (`/10`,
//! `/2`, `x1`) maps to tenths exactly, the firmware target has no FPU, and
//! `i16` tenths covers -3276.8..3276.7 C, which is well beyond anything this
//! machine can report. Formatting is the caller's business (MQTT publishes
//! `275` as `27.5`).
//!
//! [`Status`] is a thin wrapper over the raw block with named accessors
//! rather than a struct of decoded fields: every accessor is a pure function
//! of one or two words, so storing both forms would only create a way for
//! them to disagree. The raw block stays available through [`Status::raw`]
//! and [`Status::reg`] for the many registers that are not understood yet.

use crate::rtu::{self, STATUS_LEN, STATUS_START};

/// Value the heat pump reports for a sensor that is not fitted.
pub const NOT_PRESENT: u16 = 0x7fff;

/// A temperature in tenths of a degree Celsius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeciCelsius(pub i16);

impl DeciCelsius {
    /// Tenths of a degree, as read.
    #[must_use]
    pub const fn tenths(self) -> i16 {
        self.0
    }

    /// Whole degrees, truncated toward zero.
    #[must_use]
    pub const fn whole(self) -> i16 {
        self.0 / 10
    }
}

/// Decoded view of one status block poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    raw: [u16; STATUS_LEN],
}

/// `None` for the 0x7fff "not present" marker, otherwise the raw word.
#[inline]
const fn present(raw: u16) -> Option<u16> {
    if raw == NOT_PRESENT {
        None
    } else {
        Some(raw)
    }
}

impl Status {
    /// Wrap a complete status block.
    #[must_use]
    pub const fn from_block(raw: [u16; STATUS_LEN]) -> Self {
        Self { raw }
    }

    /// Wrap a slice of exactly [`STATUS_LEN`] words.
    #[must_use]
    pub fn from_words(words: &[u16]) -> Option<Self> {
        let raw: [u16; STATUS_LEN] = words.try_into().ok()?;
        Some(Self { raw })
    }

    /// Decode the heat pump's reply to [`rtu::read_status_request`].
    ///
    /// # Errors
    ///
    /// As [`rtu::parse_read_response`]: a malformed, foreign or exception
    /// frame, or a reply that does not carry 63 registers.
    pub fn from_response(frame: &[u8]) -> Result<Self, rtu::Error> {
        Ok(Self {
            raw: rtu::parse_read_block::<STATUS_LEN>(frame)?,
        })
    }

    /// The block as received, for diagnostics and the undecoded registers.
    #[must_use]
    pub const fn raw(&self) -> &[u16; STATUS_LEN] {
        &self.raw
    }

    /// Raw word at an absolute register address, or `None` outside the block.
    #[must_use]
    pub fn reg(&self, addr: u16) -> Option<u16> {
        let idx = addr.checked_sub(STATUS_START)?;
        self.raw.get(usize::from(idx)).copied()
    }

    /// Raw word at an address known to be inside the block.
    #[inline]
    fn at(&self, addr: u16) -> u16 {
        self.raw[(addr - STATUS_START) as usize]
    }

    /// Bit of a register inside the block.
    #[inline]
    fn bit(&self, addr: u16, bit: u8) -> bool {
        self.at(addr) & (1 << bit) != 0
    }

    /// Raw word scaled to tenths of a degree, `None` if not present.
    #[inline]
    fn temp(&self, addr: u16, tenths_per_raw: i16) -> Option<DeciCelsius> {
        let raw = present(self.at(addr))?;
        Some(DeciCelsius((raw as i16).wrapping_mul(tenths_per_raw)))
    }

    // --- Sensors A01-A14 (register-map.md "Sensors") ---

    /// A01 inlet water temperature, 0x000f, raw / 10.
    #[must_use]
    pub fn inlet_water(&self) -> Option<DeciCelsius> {
        self.temp(0x000f, 1)
    }

    /// A02 outlet water temperature, 0x0010, raw / 2.
    #[must_use]
    pub fn outlet_water(&self) -> Option<DeciCelsius> {
        self.temp(0x0010, 5)
    }

    /// A03 ambient temperature, 0x0011, raw / 2.
    #[must_use]
    pub fn ambient(&self) -> Option<DeciCelsius> {
        self.temp(0x0011, 5)
    }

    /// A04 exhaust temperature, 0x0015, whole degrees.
    #[must_use]
    pub fn exhaust(&self) -> Option<DeciCelsius> {
        self.temp(0x0015, 10)
    }

    /// A05 gas return temperature, 0x0013, raw / 2.
    #[must_use]
    pub fn gas_return(&self) -> Option<DeciCelsius> {
        self.temp(0x0013, 5)
    }

    /// A06 outer piping temperature, 0x0012, raw / 2.
    #[must_use]
    pub fn outer_piping(&self) -> Option<DeciCelsius> {
        self.temp(0x0012, 5)
    }

    /// A07 inner piping temperature, 0x0014, raw / 2.
    #[must_use]
    pub fn inner_piping(&self) -> Option<DeciCelsius> {
        self.temp(0x0014, 5)
    }

    /// A08 EEV aperture, 0x0018, steps.
    #[must_use]
    pub fn eev_steps(&self) -> Option<u16> {
        present(self.at(0x0018))
    }

    /// A09 compressor current, 0x0020, amps.
    #[must_use]
    pub fn compressor_current_a(&self) -> Option<u16> {
        present(self.at(0x0020))
    }

    /// A10 radiator (inverter heatsink) temperature, 0x001f, raw / 2.
    #[must_use]
    pub fn radiator(&self) -> Option<DeciCelsius> {
        self.temp(0x001f, 5)
    }

    /// A11 inverter DC bus voltage, 0x001e, volts.
    #[must_use]
    pub fn dc_bus_volts(&self) -> Option<u16> {
        present(self.at(0x001e))
    }

    /// A12 actual compressor frequency, 0x001b, Hz.
    #[must_use]
    pub fn compressor_hz(&self) -> Option<u16> {
        present(self.at(0x001b))
    }

    /// A13 fan motor speed, 0x0024, r/min.
    #[must_use]
    pub fn fan_rpm(&self) -> Option<u16> {
        present(self.at(0x0024))
    }

    /// A14 second fan motor speed, 0x0025, r/min (0 on a single-fan unit).
    #[must_use]
    pub fn fan2_rpm(&self) -> Option<u16> {
        present(self.at(0x0025))
    }

    /// Compressor target frequency, 0x001a, Hz. Not in the A menu; tracks
    /// [`Status::compressor_hz`] one step ahead ("likely" in the map).
    #[must_use]
    pub fn compressor_target_hz(&self) -> Option<u16> {
        present(self.at(0x001a))
    }

    /// Derived: the compressor is turning (frequency above zero).
    #[must_use]
    pub fn compressor_running(&self) -> bool {
        self.compressor_hz().unwrap_or(0) > 0
    }

    // --- Status and alarm bits (register-map.md "Status and alarms") ---
    //
    // Bit meanings from the vendor protocol document (docs/vendor/, "Flag
    // descriptions"), each checked against 52 h of bus captures.

    /// Water flow switch fault, 0x0008 bit 0 ("fault flags 2"). Set 10 s into
    /// the flow test, cleared when the flow returned. (0x0002 bit 1, which
    /// this used to read, sets at every thermostat stop: not a flow alarm.)
    #[must_use]
    pub fn water_flow_fault(&self) -> bool {
        self.bit(0x0008, 0)
    }

    /// High fan speed, 0x0004 bit 7. Mirrors boost (0x003f bit 6 clear) in
    /// every capture, so it doubles as the heat pump's confirmation of it.
    #[must_use]
    pub fn boost_active(&self) -> bool {
        self.bit(0x0004, 7)
    }

    /// Circulating water pump output, 0x0006 bit 2 ("output flags 3"). On
    /// with the unit, off about 45 s after a compressor stop or a power-off.
    #[must_use]
    pub fn water_pump(&self) -> bool {
        self.bit(0x0006, 2)
    }

    /// Fan output, 0x0004 bit 5. Rises 5-6 s before the fan speed reads
    /// non-zero; matches fan speed > 0 in 99.98% of the captures.
    #[must_use]
    pub fn fan_running(&self) -> bool {
        self.bit(0x0004, 5)
    }

    /// Compressor 1 output, 0x0004 bit 0: the run command, set with the
    /// target frequency and 9-12 s before the compressor turns.
    #[must_use]
    pub fn compressor_output(&self) -> bool {
        self.bit(0x0004, 0)
    }

    /// Heating demand, 0x0005 bit 7 ("output flags 2"): on while the unit
    /// wants heat, off at a thermostat stop and at a power-off.
    #[must_use]
    pub fn heating_demand(&self) -> bool {
        self.bit(0x0005, 7)
    }
}

#[cfg(test)]
pub(crate) mod snapshot {
    use super::STATUS_LEN;

    /// The status block of `docs/register-map.md` "Snapshot: status block
    /// 0x0000-0x003e", recorded 2026-10-03 15:32 while the unit was heating
    /// in ECO. Matches the A01-A14 readout at ~15:40 within the logged range.
    pub const STATUS_BLOCK: [u16; STATUS_LEN] = [
        // 0x0000
        0x2020, 0x0404, 0x0000, 0x000d, 0x0021, 0x0080, 0x0014, 0x0000, // 0x0008
        0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x7fff, 0x0113, // 0x0010
        0x003b, 0x0028, 0x000c, 0x000e, 0x0041, 0x004c, 0x7fff, 0x7fff, // 0x0018
        0x0082, 0x7fff, 0x0037, 0x0036, 0x0000, 0x0000, 0x021d, 0x0050, // 0x0020
        0x0008, 0x7fff, 0x7fff, 0x7fff, 0x02d2, 0x0000, 0x0000, 0x0000, // 0x0028
        0x0000, 0x0000, 0x7fff, 0x7fff, 0x0000, 0x7fff, 0x0000, 0x0000, // 0x0030
        0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x7fff, 0x7fff, 0x7fff, // 0x0038
        0x7fff, 0x7fff, 0x7fff, 0x7fff, 0x7fff, 0x7fff, 0x77ff,
    ];
}

#[cfg(test)]
mod tests {
    use super::{snapshot::STATUS_BLOCK, DeciCelsius, Status, NOT_PRESENT};
    use crate::rtu::{self, test_support, STATUS_LEN, STATUS_RESPONSE_LEN};

    fn snap() -> Status {
        Status::from_block(STATUS_BLOCK)
    }

    #[test]
    fn sensors_match_the_controller_readout() {
        let s = snap();
        // A01 0x000f = 0x0113 = 275 raw, / 10.
        assert_eq!(s.inlet_water(), Some(DeciCelsius(275)));
        assert_eq!(s.inlet_water().unwrap().whole(), 27);
        // A02 0x0010 = 59 raw, / 2.
        assert_eq!(s.outlet_water(), Some(DeciCelsius(295)));
        // A03 0x0011 = 40 raw, / 2 (41 -> 20.5 at the readout moment).
        assert_eq!(s.ambient(), Some(DeciCelsius(200)));
        // A04 0x0015 = 76 raw, whole degrees.
        assert_eq!(s.exhaust(), Some(DeciCelsius(760)));
        assert_eq!(s.exhaust().unwrap().whole(), 76);
        // A05 0x0013 = 14 raw, / 2.
        assert_eq!(s.gas_return(), Some(DeciCelsius(70)));
        // A06 0x0012 = 12 raw, / 2.
        assert_eq!(s.outer_piping(), Some(DeciCelsius(60)));
        // A07 0x0014 = 65 raw, / 2.
        assert_eq!(s.inner_piping(), Some(DeciCelsius(325)));
        // A10 0x001f = 80 raw, / 2.
        assert_eq!(s.radiator(), Some(DeciCelsius(400)));

        assert_eq!(s.eev_steps(), Some(130));
        assert_eq!(s.compressor_current_a(), Some(8));
        assert_eq!(s.dc_bus_volts(), Some(541));
        assert_eq!(s.compressor_hz(), Some(54));
        assert_eq!(s.compressor_target_hz(), Some(55));
        assert_eq!(s.fan_rpm(), Some(722));
        assert_eq!(s.fan2_rpm(), Some(0));
        assert!(s.compressor_running());
    }

    #[test]
    fn not_present_slots_decode_to_none() {
        let mut block = STATUS_BLOCK;
        // 0x7fff slots listed in the map: 0x0016, 0x0017, 0x0019, 0x0021-23.
        for addr in [0x000eu16, 0x0016, 0x0017, 0x0019, 0x0021, 0x0022, 0x0023] {
            assert_eq!(block[addr as usize], NOT_PRESENT, "{addr:#06x}");
        }
        // Make every named sensor read "not present" and check they go None.
        for addr in [0x000fu16, 0x0010, 0x0011, 0x0012, 0x0013, 0x0014, 0x0015] {
            block[addr as usize] = NOT_PRESENT;
        }
        for addr in [
            0x0018u16, 0x001a, 0x001b, 0x001e, 0x001f, 0x0020, 0x0024, 0x0025,
        ] {
            block[addr as usize] = NOT_PRESENT;
        }
        let s = Status::from_block(block);
        assert_eq!(s.inlet_water(), None);
        assert_eq!(s.outlet_water(), None);
        assert_eq!(s.ambient(), None);
        assert_eq!(s.exhaust(), None);
        assert_eq!(s.gas_return(), None);
        assert_eq!(s.outer_piping(), None);
        assert_eq!(s.inner_piping(), None);
        assert_eq!(s.radiator(), None);
        assert_eq!(s.eev_steps(), None);
        assert_eq!(s.compressor_current_a(), None);
        assert_eq!(s.dc_bus_volts(), None);
        assert_eq!(s.compressor_hz(), None);
        assert_eq!(s.compressor_target_hz(), None);
        assert_eq!(s.fan_rpm(), None);
        assert_eq!(s.fan2_rpm(), None);
        assert!(!s.compressor_running());
    }

    #[test]
    fn negative_temperatures_use_the_signed_raw_word() {
        let mut block = STATUS_BLOCK;
        block[0x0011] = (-8i16) as u16; // ambient -4.0 C at raw / 2
        block[0x000f] = (-25i16) as u16; // inlet -2.5 C at raw / 10
        let s = Status::from_block(block);
        assert_eq!(s.ambient(), Some(DeciCelsius(-40)));
        assert_eq!(s.inlet_water(), Some(DeciCelsius(-25)));
        assert_eq!(s.inlet_water().unwrap().whole(), -2);
    }

    #[test]
    fn snapshot_bits_are_the_logged_state() {
        // 0x0004 = 0x0021: bit 0 compressor, bit 5 fan, bit 7 clear (ECO,
        // not boost). 0x0005 = 0x0080: heating demand. 0x0006 = 0x0014:
        // bit 2 water pump.
        let s = snap();
        assert!(s.compressor_output());
        assert!(s.fan_running());
        assert!(s.water_pump());
        assert!(!s.boost_active());
        assert!(s.heating_demand());
        assert!(!s.water_flow_fault());
    }

    #[test]
    fn boost_bit_follows_the_capture() {
        // 0x0004 goes 33 -> 161 when full power engages (0x0021 | 0x80).
        let mut block = STATUS_BLOCK;
        block[0x0004] = 161;
        let s = Status::from_block(block);
        assert!(s.boost_active());
        assert!(s.compressor_output());
        assert!(s.fan_running());
    }

    #[test]
    fn water_flow_fault_sequence() {
        // 16:57:15 0x0002 bit 1 sets (thermostat-stop flag, not the fault);
        // 16:57:25 compressor and heating demand clear and 0x0008 bit 0
        // sets; 16:58:10 pump and fan off.
        let mut block = STATUS_BLOCK;
        block[0x0002] = 0x0002;
        let s = Status::from_block(block);
        assert!(!s.water_flow_fault());
        assert!(s.heating_demand());

        block[0x0004] &= !0x0001;
        block[0x0005] &= !0x0080;
        block[0x0008] |= 0x0001;
        let s = Status::from_block(block);
        assert!(s.water_flow_fault());
        assert!(!s.compressor_output());
        assert!(!s.heating_demand());
        assert!(s.water_pump());

        block[0x0004] &= !0x0020;
        block[0x0006] = 0;
        let s = Status::from_block(block);
        assert!(!s.fan_running());
        assert!(!s.water_pump());
    }

    #[test]
    fn raw_block_stays_reachable() {
        let s = snap();
        assert_eq!(s.raw(), &STATUS_BLOCK);
        assert_eq!(s.reg(0x0000), Some(0x2020));
        assert_eq!(s.reg(0x003e), Some(0x77ff));
        assert_eq!(s.reg(0x003f), None);
        // Undecoded registers are still readable.
        assert_eq!(s.reg(0x0006), Some(0x0014));
    }

    #[test]
    fn decodes_a_real_shaped_response() {
        let mut buf = [0u8; STATUS_RESPONSE_LEN];
        let n = test_support::read_response(&STATUS_BLOCK, &mut buf);
        assert_eq!(n, STATUS_RESPONSE_LEN);
        // 126 data bytes, like the capture says.
        assert_eq!(buf[2], 126);
        let s = Status::from_response(&buf).unwrap();
        assert_eq!(s, snap());

        // A truncated block is refused rather than silently zero-filled.
        let short = &STATUS_BLOCK[..STATUS_LEN - 1];
        let n = test_support::read_response(short, &mut buf);
        assert!(matches!(
            Status::from_response(&buf[..n]),
            Err(rtu::Error::BadByteCount(124))
        ));
        assert!(Status::from_words(short).is_none());
        assert_eq!(Status::from_words(&STATUS_BLOCK), Some(snap()));
    }
}
