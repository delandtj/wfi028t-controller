//! Persistence of the runtime bus settings and the operating mode.
//!
//! The records live at the start of the ESP-IDF **`nvs` data partition**, which
//! the espflash default partition table puts at 0x9000 with a size of 0x6000.
//! We do not use IDF's NVS library at all, so that partition's raw space is
//! ours to format; the partition table is read at boot through
//! esp-bootloader-esp-idf's API rather than hard-coding 0x9000, so a custom
//! partition table moves the records along with the partition.
//!
//! Only the first flash sector (4096 bytes) of the partition is touched:
//!
//! | Offset | Bytes | Record |
//! |---|---|---|
//! | 0 | 16 | bus configuration, [`modbus_sniffer_core::encode_bus_record`] |
//! | 16 | 8 | operating mode, [`encode_mode_record`] |
//! | 24 | 112 | MQTT broker, [`crate::mqtt::config::encode_record`] |
//!
//! A write erases the sector and lays all records down again, so the in-memory
//! copy of the others has to be current - which is why [`init`] keeps what it
//! read. Anything else (erased flash, a record from a future version, bit rot
//! caught by the CRC) reads back as "no setting" and the firmware falls back to
//! [`crate::DEFAULT_BUS`], [`OpMode::Listen`] - the safe mode, which never
//! transmits - and the build-time MQTT configuration.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_bootloader_esp_idf::partitions::{
    self, DataPartitionSubType, PartitionEntry, PartitionType,
};
use esp_storage::FlashStorage;

use modbus_sniffer_core as sniffer;
use sniffer::BusConfig;

use crate::master::OpMode;
use crate::mqtt::{self, config as mqtt_config, Stored};

/// The smallest erasable unit of the flash chip.
const SECTOR: u32 = FlashStorage::SECTOR_SIZE;

/// Offset of the mode record inside the sector, right after the 16-byte bus
/// record.
const MODE_OFFSET: u32 = sniffer::BUS_RECORD_LEN as u32;

/// Size of the on-flash mode record. A multiple of the flash word size (4).
const MODE_RECORD_LEN: usize = 8;

/// Offset of the MQTT broker record, right after the mode record.
const MQTT_OFFSET: u32 = MODE_OFFSET + MODE_RECORD_LEN as u32;

/// Magic at the start of the mode record: "WCM1" (Wfi Controller Mode, rev 1).
const MODE_RECORD_MAGIC: u32 = u32::from_le_bytes(*b"WCM1");

/// Record format version. Bump when the layout changes.
const MODE_RECORD_VERSION: u8 = 1;

/// Serialise the operating mode into its on-flash form.
///
/// Layout, little endian: magic u32, version u8, mode u8, CRC-16 of bytes 0..6.
fn encode_mode_record(mode: OpMode) -> [u8; MODE_RECORD_LEN] {
    let mut raw = [0u8; MODE_RECORD_LEN];
    raw[0..4].copy_from_slice(&MODE_RECORD_MAGIC.to_le_bytes());
    raw[4] = MODE_RECORD_VERSION;
    raw[5] = mode.code();
    let crc = sniffer::crc16(&raw[0..6]);
    raw[6..8].copy_from_slice(&crc.to_le_bytes());
    raw
}

/// Parse an on-flash mode record. `None` for anything that is not a valid,
/// current record - including erased (all-0xff) flash.
fn decode_mode_record(raw: &[u8]) -> Option<OpMode> {
    if raw.len() < MODE_RECORD_LEN {
        return None;
    }
    if u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) != MODE_RECORD_MAGIC {
        return None;
    }
    if raw[4] != MODE_RECORD_VERSION {
        return None;
    }
    if u16::from_le_bytes([raw[6], raw[7]]) != sniffer::crc16(&raw[0..6]) {
        return None;
    }
    OpMode::from_code(raw[5])
}

struct Store {
    flash: FlashStorage<'static>,
    /// The partition the records live in, or `None` if the table has no `nvs`
    /// data partition - in which case settings simply do not persist and the
    /// `bus` and `mode` commands say so instead of pretending.
    area: Option<PartitionEntry>,
    /// Last known values, so one record can be rewritten without losing the
    /// others (an erase takes the whole sector).
    bus: BusConfig,
    mode: OpMode,
    broker: Stored,
}

/// Guards the one flash peripheral. An async mutex, because the command
/// handlers that write the records are async and a flash erase takes tens of
/// milliseconds - long enough that spinning would be rude to the bus task.
static STORE: Mutex<CriticalSectionRawMutex, Option<Store>> = Mutex::new(None);

/// Take ownership of the flash peripheral, locate the settings area and return
/// the stored bus configuration and operating mode (or the defaults).
///
/// The stored MQTT configuration is handed to [`crate::mqtt::load`] here
/// rather than returned, because `main` has nothing to do with it.
///
/// Call once, from `main`, before anything else touches flash.
pub async fn init(flash: esp_hal::peripherals::FLASH<'static>) -> (BusConfig, OpMode) {
    let mut flash = FlashStorage::new(flash);

    // 3 KB on the main task's stack, which is the linker-provided one.
    let mut table = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let area = partitions::read_partition_table(&mut flash, &mut table)
        .ok()
        .and_then(|table| {
            table
                .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
                .ok()
                .flatten()
        })
        .filter(|entry| entry.len() >= SECTOR);

    let mut bus = crate::DEFAULT_BUS;
    let mut mode = OpMode::Listen;
    let mut broker = Stored::Unset;
    if let Some(entry) = area {
        let mut region = entry.as_flash_region(&mut flash);
        let mut raw = [0u8; sniffer::BUS_RECORD_LEN];
        if region.read(0, &mut raw).is_ok() {
            if let Some(stored) = sniffer::decode_bus_record(&raw) {
                bus = stored;
            }
        }
        let mut raw = [0u8; MODE_RECORD_LEN];
        if region.read(MODE_OFFSET, &mut raw).is_ok() {
            if let Some(stored) = decode_mode_record(&raw) {
                mode = stored;
            }
        }
        let mut raw = [0u8; mqtt_config::RECORD_LEN];
        if region.read(MQTT_OFFSET, &mut raw).is_ok() {
            broker = mqtt_config::decode_record(&raw);
        }
    }

    *STORE.lock().await = Some(Store {
        flash,
        area,
        bus,
        mode,
        broker,
    });
    mqtt::load(broker);
    (bus, mode)
}

/// Persist a bus configuration. The `Err` payload is protocol-visible text.
pub async fn store_bus(bus: BusConfig) -> Result<(), &'static str> {
    write_records(Some(bus), None, None).await
}

/// Persist the operating mode. The `Err` payload is protocol-visible text.
pub async fn store_mode(mode: OpMode) -> Result<(), &'static str> {
    write_records(None, Some(mode), None).await
}

/// Persist the MQTT broker setting. The `Err` payload is protocol-visible
/// text.
pub async fn store_broker(broker: Stored) -> Result<(), &'static str> {
    write_records(None, None, Some(broker)).await
}

/// Erase the sector and lay every record down again, with `bus`, `mode`
/// and/or `broker` replaced.
async fn write_records(
    bus: Option<BusConfig>,
    mode: Option<OpMode>,
    broker: Option<Stored>,
) -> Result<(), &'static str> {
    let mut guard = STORE.lock().await;
    let store = guard.as_mut().ok_or("settings store not initialised")?;
    let entry = store.area.ok_or("no nvs data partition to save into")?;

    let next_bus = bus.unwrap_or(store.bus);
    let next_mode = mode.unwrap_or(store.mode);
    let next_broker = broker.unwrap_or(store.broker);
    let bus_record = sniffer::encode_bus_record(next_bus);
    let mode_record = encode_mode_record(next_mode);
    // `Stored::Unset` has no record: an erased slot is what it means, so
    // nothing is written there and the build-time default applies again.
    let broker_record = mqtt_config::encode_record(next_broker);

    let mut region = entry.as_flash_region(&mut store.flash);
    region.erase(0, SECTOR).map_err(|_| "flash erase failed")?;
    region
        .write(0, &bus_record)
        .map_err(|_| "flash write failed")?;
    region
        .write(MODE_OFFSET, &mode_record)
        .map_err(|_| "flash write failed")?;
    if let Some(record) = broker_record {
        region
            .write(MQTT_OFFSET, &record)
            .map_err(|_| "flash write failed")?;
    }

    store.bus = next_bus;
    store.mode = next_mode;
    store.broker = next_broker;
    Ok(())
}
