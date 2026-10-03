//! Typed register model for the WFI-028T/035T full-inverter pool heat pump.
//!
//! The heat pump PCB is Modbus RTU slave 0x01 on the wired controller's
//! RS-485 bus (9600 8N1). The stock controller polls two register blocks once
//! per second and rewrites the whole settings block on every change. This
//! crate owns the wire format ([`rtu`]) and the meaning of the two blocks
//! ([`status`], [`settings`]), so the firmware's bus master only has to deal
//! with UART bytes and timing.
//!
//! Source of truth: `docs/register-map.md`. Only entries marked "confirmed"
//! or "likely" there are exposed as named accessors; everything else stays
//! reachable as raw words.
//!
//! `no_std`, no allocator, no dependencies (see [`rtu::crc16`] for why the
//! CRC is local). `std` is pulled in only for the test build.
//!
//! # Example
//!
//! ```
//! use hp_model::{rtu, settings::{Command, Settings}};
//!
//! // Ask for the settings block.
//! let req = rtu::read_settings_request();
//! assert_eq!(&req[..6], &[0x01, 0x03, 0x00, 0x3f, 0x00, 0x43]);
//!
//! // Decode the reply, change one thing, send the whole block back.
//! # let reply = {
//! #     let mut f = [0u8; 139];
//! #     f[0] = 0x01; f[1] = 0x03; f[2] = 134; f[3] = 0x10; f[4] = 0x71;
//! #     let c = rtu::crc16(&f[..137]);
//! #     f[137] = c as u8; f[138] = (c >> 8) as u8;
//! #     f
//! # };
//! let current = Settings::from_response(&reply).unwrap();
//! assert!(current.power());
//! let wanted = current.apply(Command::SetHeatSetpoint(30)).unwrap();
//! let frame = wanted.write_request();
//! assert_eq!(&frame[..7], &[0x01, 0x10, 0x00, 0x3f, 0x00, 0x43, 134]);
//! ```
#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]

pub mod rtu;
pub mod settings;
pub mod status;

pub use rtu::{Error as RtuError, SETTINGS_LEN, SETTINGS_START, SLAVE, STATUS_LEN, STATUS_START};
pub use settings::{Command, Field, Mode, Rejected, Settings};
pub use status::{DeciCelsius, Status};

/// The ADR's freshness rule: a settings block older than this must not be
/// written back, because the heat pump may have changed registers we do not
/// understand in the meantime.
///
/// See `docs/adr/0001-rust-replacement-controller.md`, "Fails safe".
pub const MAX_SETTINGS_AGE_MS: u32 = 2_000;

/// `true` if a settings block read `age_ms` ago is still fresh enough to be
/// written back. The crate keeps no clock of its own; the caller measures the
/// age.
#[must_use]
pub const fn write_allowed(age_ms: u32) -> bool {
    age_ms < MAX_SETTINGS_AGE_MS
}

#[cfg(test)]
mod freshness_tests {
    use super::{write_allowed, MAX_SETTINGS_AGE_MS};

    #[test]
    fn freshness_boundary() {
        assert!(write_allowed(0));
        assert!(write_allowed(MAX_SETTINGS_AGE_MS - 1));
        assert!(!write_allowed(MAX_SETTINGS_AGE_MS));
        assert!(!write_allowed(u32::MAX));
    }
}
