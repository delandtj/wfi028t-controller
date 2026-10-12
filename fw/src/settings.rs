//! Persistence of the runtime bus settings, the operating mode and the
//! provisioning records.
//!
//! The records live at the start of the ESP-IDF **`nvs` data partition**, which
//! the espflash default partition table puts at 0x9000 with a size of 0x6000.
//! We do not use IDF's NVS library at all, so that partition's raw space is
//! ours to format; the partition table is read at boot through
//! esp-bootloader-esp-idf's API rather than hard-coding 0x9000, so a custom
//! partition table moves the records along with the partition.
//!
//! Only the first flash sector (4096 bytes) of the partition is touched. The
//! offsets are [`provision::layout`]'s, which is also where the host tests
//! that hold them in place live:
//!
//! | Offset | Bytes | Record |
//! |---|---|---|
//! | 0 | 16 | bus configuration, [`modbus_sniffer_core::encode_bus_record`] |
//! | 16 | 8 | operating mode, [`encode_mode_record`] |
//! | 24 | 112 | MQTT broker, [`crate::mqtt::config::encode_record`] |
//! | 136 | 112 | WiFi station, [`provision::records::encode_wifi`] |
//! | 248 | 64 | admin password, [`provision::records::encode_admin`] |
//! | 312 | 48 | device, [`provision::records::encode_device`] |
//!
//! A write erases the sector and lays all records down again, so the in-memory
//! copy of the others has to be current - which is why [`init`] keeps what it
//! read. Anything else (erased flash, a record from a future version, bit rot
//! caught by the CRC) reads back as "no setting" and the firmware falls back to
//! [`crate::DEFAULT_BUS`], [`OpMode::Listen`] - the safe mode, which never
//! transmits - the build-time MQTT configuration, the build-time WiFi
//! credentials and the build-time hostname.
//!
//! The three records ADR 0004 added are appended after the MQTT record, so a
//! controller updated in place keeps its bus, mode and MQTT settings: the new
//! offsets were erased space in every image before this one.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_bootloader_esp_idf::partitions::{
    self, DataPartitionSubType, PartitionEntry, PartitionType,
};
use esp_storage::FlashStorage;

use modbus_sniffer_core as sniffer;
use provision::layout;
use provision::records::{self, Admin, Device, Wifi};
use sniffer::BusConfig;

use crate::master::OpMode;
use crate::mqtt::{self, config as mqtt_config, Stored};

/// The smallest erasable unit of the flash chip.
const SECTOR: u32 = FlashStorage::SECTOR_SIZE;

/// Size of the on-flash mode record. A multiple of the flash word size (4).
const MODE_RECORD_LEN: usize = 8;

// The layout crate has no business depending on the sniffer or the MQTT
// module, so it carries the two older lengths as plain numbers. These are
// what keep the two descriptions of one sector honest.
const _: () = assert!(sniffer::BUS_RECORD_LEN == layout::BUS_LEN);
const _: () = assert!(MODE_RECORD_LEN == layout::MODE_LEN);
const _: () = assert!(mqtt_config::RECORD_LEN == layout::MQTT_LEN);
const _: () = assert!(layout::SECTOR_LEN as u32 == SECTOR);

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
    /// `None` is "no record", which for these three means the build-time
    /// default applies (WiFi, hostname) or the device is unconfigured
    /// (admin).
    wifi: Option<Wifi>,
    admin: Option<Admin>,
    device: Option<Device>,
}

/// Whether an admin password record was found, for `status`.
///
/// An atomic rather than a field in [`STORE`], because the status line is
/// built synchronously and must never wait on a flash write.
static ADMIN_PRESENT: AtomicBool = AtomicBool::new(false);

/// Whether the device has an admin password, which is ADR 0004's definition
/// of "configured" - the thing that decides whether setup mode opens.
#[must_use]
pub fn configured() -> bool {
    ADMIN_PRESENT.load(Ordering::Relaxed)
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
    let mut wifi = None;
    let mut admin = None;
    let mut device = None;
    if let Some(entry) = area {
        let mut region = entry.as_flash_region(&mut flash);
        let mut raw = [0u8; layout::BUS_LEN];
        if region.read(layout::BUS_OFFSET, &mut raw).is_ok() {
            if let Some(stored) = sniffer::decode_bus_record(&raw) {
                bus = stored;
            }
        }
        let mut raw = [0u8; layout::MODE_LEN];
        if region.read(layout::MODE_OFFSET, &mut raw).is_ok() {
            if let Some(stored) = decode_mode_record(&raw) {
                mode = stored;
            }
        }
        let mut raw = [0u8; layout::MQTT_LEN];
        if region.read(layout::MQTT_OFFSET, &mut raw).is_ok() {
            broker = mqtt_config::decode_record(&raw);
        }
        let mut raw = [0u8; layout::WIFI_LEN];
        if region.read(layout::WIFI_OFFSET, &mut raw).is_ok() {
            wifi = records::decode_wifi(&raw);
        }
        let mut raw = [0u8; layout::ADMIN_LEN];
        if region.read(layout::ADMIN_OFFSET, &mut raw).is_ok() {
            admin = records::decode_admin(&raw);
        }
        let mut raw = [0u8; layout::DEVICE_LEN];
        if region.read(layout::DEVICE_OFFSET, &mut raw).is_ok() {
            device = records::decode_device(&raw);
        }
    }

    ADMIN_PRESENT.store(admin.is_some(), Ordering::Relaxed);
    *STORE.lock().await = Some(Store {
        flash,
        area,
        bus,
        mode,
        broker,
        wifi,
        admin,
        device,
    });
    mqtt::load(broker);
    (bus, mode)
}

/// The stored WiFi credentials, if any. `None` means the build-time
/// `SNIFFER_WIFI_*` applies.
pub async fn stored_wifi() -> Option<Wifi> {
    STORE.lock().await.as_ref().and_then(|store| store.wifi)
}

/// The stored device record, if any. `None` means the build-time hostname
/// applies and the line ports keep their default policy.
pub async fn stored_device() -> Option<Device> {
    STORE.lock().await.as_ref().and_then(|store| store.device)
}

/// The stored admin password, if any, for ADR 0003's login check.
///
/// No caller yet: the records come before the web server that reads them and
/// the button task that erases them, so that an in-place update starts
/// keeping the new settings before anything can set them.
#[allow(dead_code)]
pub async fn stored_admin() -> Option<Admin> {
    STORE.lock().await.as_ref().and_then(|store| store.admin)
}

/// Run `f` with exclusive use of the flash peripheral.
///
/// The OTA receiver and the probation check ([`crate::ota`]) write app slots
/// and `otadata`; they go through this rather than owning flash themselves,
/// because there is one flash peripheral and the same [`STORE`] lock has to
/// cover both. That is also what the ADR means by "a settings write in
/// flight finishes before the reboot": an OTA sector write and a `set`'s
/// settings write can never overlap, in either order.
///
/// `f` is synchronous on purpose: every flash operation blocks the CPU
/// anyway, and holding the lock across an await would let an OTA transfer
/// stall `bus`, `mode` or `mqtt` for the length of a whole image.
pub async fn with_flash<R>(
    f: impl FnOnce(&mut FlashStorage<'static>) -> R,
) -> Result<R, &'static str> {
    let mut guard = STORE.lock().await;
    let store = guard.as_mut().ok_or("settings store not initialised")?;
    Ok(f(&mut store.flash))
}

/// What a write changes. `None` is "leave this record as it is"; for the
/// three records that can be absent, `Some(None)` is "drop it", which is what
/// `wifi clear` and [`factory_erase`] need and why those fields are nested.
#[derive(Default)]
struct Changes {
    bus: Option<BusConfig>,
    mode: Option<OpMode>,
    broker: Option<Stored>,
    wifi: Option<Option<Wifi>>,
    admin: Option<Option<Admin>>,
    device: Option<Option<Device>>,
}

/// Persist a bus configuration. The `Err` payload is protocol-visible text.
pub async fn store_bus(bus: BusConfig) -> Result<(), &'static str> {
    write_records(Changes {
        bus: Some(bus),
        ..Changes::default()
    })
    .await
}

/// Persist the operating mode. The `Err` payload is protocol-visible text.
pub async fn store_mode(mode: OpMode) -> Result<(), &'static str> {
    write_records(Changes {
        mode: Some(mode),
        ..Changes::default()
    })
    .await
}

/// Persist the MQTT broker setting. The `Err` payload is protocol-visible
/// text.
pub async fn store_broker(broker: Stored) -> Result<(), &'static str> {
    write_records(Changes {
        broker: Some(broker),
        ..Changes::default()
    })
    .await
}

/// Persist the WiFi credentials, or `None` to drop the record so the
/// build-time `SNIFFER_WIFI_*` applies again. Takes effect at the next
/// reboot: the radio is configured once, at boot.
pub async fn store_wifi(wifi: Option<Wifi>) -> Result<(), &'static str> {
    write_records(Changes {
        wifi: Some(wifi),
        ..Changes::default()
    })
    .await
}

/// Persist the admin password, or `None` to drop it - which makes the device
/// unconfigured again.
///
/// No caller yet: ADR 0003's setup page is what produces the salt, the
/// iteration count and the hash. This side only stores them.
#[allow(dead_code)]
pub async fn store_admin(admin: Option<Admin>) -> Result<(), &'static str> {
    write_records(Changes {
        admin: Some(admin),
        ..Changes::default()
    })
    .await
}

/// Persist the device record (hostname, line-port policy, setup flag), or
/// `None` to drop it. The hostname takes effect at the next reboot.
pub async fn store_device(device: Option<Device>) -> Result<(), &'static str> {
    write_records(Changes {
        device: Some(device),
        ..Changes::default()
    })
    .await
}

/// Factory reset, as ADR 0004 defines it: drop the WiFi, admin, MQTT and
/// device records and set the mode to listen. The bus record stays - it
/// describes the wiring, not the owner.
///
/// Flash only. Nothing in RAM is changed and the bus is not touched, because
/// the caller (the BOOT-button task, which comes later) reboots straight
/// afterwards and the boot path then reads the erased sector. That task is
/// also why this has no caller yet.
#[allow(dead_code)]
pub async fn factory_erase() -> Result<(), &'static str> {
    write_records(Changes {
        bus: None,
        mode: Some(OpMode::Listen),
        broker: Some(Stored::Unset),
        wifi: Some(None),
        admin: Some(None),
        device: Some(None),
    })
    .await
}

/// Erase the sector and lay every record down again, with `changes` applied.
async fn write_records(changes: Changes) -> Result<(), &'static str> {
    let mut guard = STORE.lock().await;
    let store = guard.as_mut().ok_or("settings store not initialised")?;
    let entry = store.area.ok_or("no nvs data partition to save into")?;

    let next_bus = changes.bus.unwrap_or(store.bus);
    let next_mode = changes.mode.unwrap_or(store.mode);
    let next_broker = changes.broker.unwrap_or(store.broker);
    let next_wifi = changes.wifi.unwrap_or(store.wifi);
    let next_admin = changes.admin.unwrap_or(store.admin);
    let next_device = changes.device.unwrap_or(store.device);

    let bus_record = sniffer::encode_bus_record(next_bus);
    let mode_record = encode_mode_record(next_mode);
    // `Stored::Unset` and the three `None`s have no record: an erased slot is
    // what they mean, so nothing is written there and the build-time default
    // applies again.
    let broker_record = mqtt_config::encode_record(next_broker);
    let wifi_record = next_wifi.as_ref().map(records::encode_wifi);
    let admin_record = next_admin.as_ref().map(records::encode_admin);
    let device_record = next_device.as_ref().map(records::encode_device);

    let mut region = entry.as_flash_region(&mut store.flash);
    region.erase(0, SECTOR).map_err(|_| "flash erase failed")?;
    region
        .write(layout::BUS_OFFSET, &bus_record)
        .map_err(|_| "flash write failed")?;
    region
        .write(layout::MODE_OFFSET, &mode_record)
        .map_err(|_| "flash write failed")?;
    for (offset, record) in [
        (layout::MQTT_OFFSET, broker_record.as_ref().map(|r| &r[..])),
        (layout::WIFI_OFFSET, wifi_record.as_ref().map(|r| &r[..])),
        (layout::ADMIN_OFFSET, admin_record.as_ref().map(|r| &r[..])),
        (
            layout::DEVICE_OFFSET,
            device_record.as_ref().map(|r| &r[..]),
        ),
    ] {
        if let Some(record) = record {
            region
                .write(offset, record)
                .map_err(|_| "flash write failed")?;
        }
    }

    store.bus = next_bus;
    store.mode = next_mode;
    store.broker = next_broker;
    store.wifi = next_wifi;
    store.admin = next_admin;
    store.device = next_device;
    ADMIN_PRESENT.store(next_admin.is_some(), Ordering::Relaxed);
    Ok(())
}
