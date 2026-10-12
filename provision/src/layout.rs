//! Where every record sits in the settings sector.
//!
//! One 4 KiB flash sector in the `nvs` partition holds them all, and a write
//! erases the sector and lays every record down again
//! (`fw/src/settings.rs`). The offsets are therefore a single contract
//! between the firmware and whatever tool ever reads a dumped sector, which
//! is why they live here rather than next to each codec.
//!
//! | Offset | Bytes | Record | Codec |
//! |---|---|---|---|
//! | 0 | 16 | bus configuration | `modbus_sniffer_core::encode_bus_record` |
//! | 16 | 8 | operating mode | `fw::settings::encode_mode_record` |
//! | 24 | 112 | MQTT broker | `fw::mqtt::config::encode_record` |
//! | 136 | 112 | WiFi station | [`crate::records::encode_wifi`] |
//! | 248 | 64 | admin password | [`crate::records::encode_admin`] |
//! | 312 | 48 | device | [`crate::records::encode_device`] |
//!
//! ADR 0004 promises that a controller updated in place keeps its bus, mode
//! and MQTT settings, so the first three offsets can never move: the new
//! records are appended after the MQTT record, into space that an older
//! firmware left erased. [`RECORDS`] and the tests below are what hold that
//! promise up.
//!
//! Every offset and length is a multiple of 4, the flash word size: the
//! `esp-storage` region writer takes word-aligned offsets and lengths only.

/// The smallest erasable unit of the flash chip, and the whole of the space
/// these records may use.
pub const SECTOR_LEN: usize = 4096;

/// Bus configuration: baud rate and frame format (ADR 0001).
pub const BUS_OFFSET: u32 = 0;
/// Length of the bus record, `modbus_sniffer_core::BUS_RECORD_LEN`.
pub const BUS_LEN: usize = 16;

/// Operating mode: listen or master.
pub const MODE_OFFSET: u32 = 16;
/// Length of the mode record.
pub const MODE_LEN: usize = 8;

/// MQTT broker: address, port and credentials.
pub const MQTT_OFFSET: u32 = 24;
/// Length of the MQTT record, `fw::mqtt::config::RECORD_LEN`.
pub const MQTT_LEN: usize = 112;

/// WiFi station credentials (ADR 0004).
pub const WIFI_OFFSET: u32 = 136;
/// Length of the WiFi record, [`crate::records::WIFI_RECORD_LEN`].
pub const WIFI_LEN: usize = 112;

/// Admin password: salt, iteration count and hash (ADR 0003).
pub const ADMIN_OFFSET: u32 = 248;
/// Length of the admin record, [`crate::records::ADMIN_RECORD_LEN`].
pub const ADMIN_LEN: usize = 64;

/// Device identity and policy: hostname, line ports, setup complete.
pub const DEVICE_OFFSET: u32 = 312;
/// Length of the device record, [`crate::records::DEVICE_RECORD_LEN`].
pub const DEVICE_LEN: usize = 48;

/// Every record in the sector, in offset order: name, offset, length.
///
/// The firmware does not iterate over this - it reads and writes each record
/// by name - but a dump tool can, and the tests use it to check the layout as
/// a whole rather than one constant at a time.
pub const RECORDS: [(&str, u32, usize); 6] = [
    ("bus", BUS_OFFSET, BUS_LEN),
    ("mode", MODE_OFFSET, MODE_LEN),
    ("mqtt", MQTT_OFFSET, MQTT_LEN),
    ("wifi", WIFI_OFFSET, WIFI_LEN),
    ("admin", ADMIN_OFFSET, ADMIN_LEN),
    ("device", DEVICE_OFFSET, DEVICE_LEN),
];

/// First byte of the sector no record uses. Where a later ADR's record goes.
pub const END: u32 = DEVICE_OFFSET + DEVICE_LEN as u32;

#[cfg(test)]
mod tests {
    use super::{
        ADMIN_LEN, ADMIN_OFFSET, BUS_LEN, BUS_OFFSET, DEVICE_LEN, DEVICE_OFFSET, END, MODE_LEN,
        MODE_OFFSET, MQTT_LEN, MQTT_OFFSET, RECORDS, SECTOR_LEN, WIFI_LEN, WIFI_OFFSET,
    };

    /// The literals ADR 0002's firmware wrote. Spelled out rather than
    /// derived, because the point of the test is that nothing derives them
    /// into a different place: a controller in the field has these bytes at
    /// these offsets.
    #[test]
    fn the_old_offsets_never_move() {
        assert_eq!((BUS_OFFSET, BUS_LEN), (0, 16));
        assert_eq!((MODE_OFFSET, MODE_LEN), (16, 8));
        assert_eq!((MQTT_OFFSET, MQTT_LEN), (24, 112));
    }

    #[test]
    fn the_new_records_come_after_the_old_ones() {
        assert!(WIFI_OFFSET >= MQTT_OFFSET + MQTT_LEN as u32);
        assert!(ADMIN_OFFSET >= WIFI_OFFSET + WIFI_LEN as u32);
        assert!(DEVICE_OFFSET >= ADMIN_OFFSET + ADMIN_LEN as u32);
    }

    #[test]
    fn nothing_overlaps_and_the_table_is_in_order() {
        let mut next = 0u32;
        for (name, offset, len) in RECORDS {
            assert!(
                offset >= next,
                "{name} at {offset} overlaps the record before it"
            );
            next = offset + len as u32;
        }
        assert_eq!(next, END);
    }

    #[test]
    fn everything_is_word_aligned_and_fits_the_sector() {
        for (name, offset, len) in RECORDS {
            assert_eq!(offset % 4, 0, "{name} offset is not word aligned");
            assert_eq!(len % 4, 0, "{name} length is not a multiple of 4");
            assert!(len > 0, "{name} is empty");
        }
        assert_eq!(END % 4, 0);
        assert!(
            END as usize <= SECTOR_LEN,
            "the records need {END} bytes, the sector has {SECTOR_LEN}"
        );
    }

    #[test]
    fn the_lengths_match_the_codecs() {
        assert_eq!(WIFI_LEN, crate::records::WIFI_RECORD_LEN);
        assert_eq!(ADMIN_LEN, crate::records::ADMIN_RECORD_LEN);
        assert_eq!(DEVICE_LEN, crate::records::DEVICE_RECORD_LEN);
    }
}
