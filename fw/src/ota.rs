//! Firmware updates over the network: the receiver on TCP 4002, the new
//! image's self-confirmation, and the planned-reboot marker (ADR 0002,
//! components 3, 4 and 5).
//!
//! The flash holds two app slots ([`crate`]'s `partitions.csv`). An update
//! writes the slot that is not running, verifies it, and only then points
//! `otadata` at it; the second-stage bootloader
//! (`fw/bootloader/esp32c6-bootloader-rollback.bin`, built with
//! `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`) boots it once as
//! `PendingVerify`. If it does not mark itself `Valid` - because it crashed,
//! wedged the watchdog, or failed its own checks inside
//! [`PROBATION_LIMIT`] - the next boot rolls back to the slot that worked.
//!
//! | Stage | Where |
//! |---|---|
//! | wire format, signature | [`header`] (pure, host-tested) |
//! | receive, verify, write | [`ota_task`] |
//! | probation, confirmation | [`probation_task`] |
//! | planned reboot | [`planned_reset`], [`boot_check`] |
//!
//! Authentication is one ed25519 signature over the header, checked against
//! [`PUBLIC_KEY`] - `fw/ota-signing.pub`, compiled in. The image is bound to
//! that header by its SHA-256, which is computed as the bytes are written, so
//! nothing unsigned is ever booted even though the signature covers 140 bytes
//! instead of a megabyte. The host side is `tools/fw-ota`.
//!
//! Three things deliberately do not happen here:
//!
//! - **No erase before the checks.** Magic, version, target and signature are
//!   settled before a single sector is touched, so a bad push leaves the
//!   standby slot (and the running one) exactly as they were.
//! - **No `otadata` write before the image verifies.** A power cut mid-update
//!   leaves a half-written standby slot that nothing will ever boot.
//! - **No polling pause.** The bus task keeps the heat pump polled through
//!   the whole transfer; flash work is done one 4 KB sector at a time (~40 ms
//!   each) and takes the same lock as a settings write, so the two can never
//!   overlap.

pub mod header;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read as _, Write as _};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, PartitionEntry, PartitionType,
};
use esp_hal::rtc_cntl::SocResetReason;
use esp_storage::FlashStorage;
use sha2::{Digest, Sha256};
use static_cell::StaticCell;

use modbus_sniffer_core as sniffer;
use sniffer::Line;

use crate::master;
use crate::mqtt;
use header::{Header, HEADER_LEN, PUBLIC_KEY_LEN, TARGET};

/// Where an image is pushed. Separate from the line interface (4000) and the
/// console (4001): this one speaks a binary protocol and nothing else.
pub const OTA_PORT: u16 = 4002;

/// The key an image has to be signed with, committed as a public key and
/// compiled in. Replacing it means a USB flash, which is the point.
static PUBLIC_KEY: [u8; PUBLIC_KEY_LEN] = *include_bytes!("../ota-signing.pub");

/// Flash sector: the erase unit, the write unit, and the chunk read off the
/// socket at a time.
const SECTOR: u32 = FlashStorage::SECTOR_SIZE;

/// Progress line every this many bytes written.
const PROGRESS_EVERY: u32 = 64 * 1024;

/// How long the header may take to arrive once a client has connected.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one sector's worth of image may take to arrive.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Let the last lines reach the client (and the capture log) before the
/// reboot takes the network down.
const REBOOT_DELAY: Duration = Duration::from_millis(400);

/// How long a new image has to prove itself before it is rolled back.
const PROBATION_LIMIT: Duration = Duration::from_secs(120);

/// Clean polls in a row that confirm an image in master mode.
const CLEAN_POLLS: u32 = 10;

/// Frames that confirm an image in listen mode.
const LISTEN_FRAMES: u32 = 10;

/// How long a quiet bus with no UART error confirms an image in listen mode.
const LISTEN_QUIET: Duration = Duration::from_secs(30);

/// How often the probation checks are re-evaluated if no snapshot arrives.
const PROBATION_TICK: Duration = Duration::from_millis(500);

/// Socket buffers. RX takes one sector at a time; TX only ever carries the
/// short `ok`/`err` lines.
const OTA_RX_BUF: usize = 4096;
const OTA_TX_BUF: usize = 512;

static OTA_RX: StaticCell<[u8; OTA_RX_BUF]> = StaticCell::new();
static OTA_TX: StaticCell<[u8; OTA_TX_BUF]> = StaticCell::new();

// ---------------------------------------------------------------------------
// Reported state
// ---------------------------------------------------------------------------

/// What `status`'s `ota=` field says. The codes are this module's own, not
/// the on-flash ones.
const STATE_UNKNOWN: u8 = 0;
const STATE_UNDEFINED: u8 = 1;
const STATE_PENDING: u8 = 2;
const STATE_VALID: u8 = 3;
const STATE_INVALID: u8 = 4;
const STATE_ABORTED: u8 = 5;

static OTA_STATE: AtomicU8 = AtomicU8::new(STATE_UNKNOWN);

/// True once an OTA transfer is far enough along that a reboot is coming.
static REBOOTING: AtomicBool = AtomicBool::new(false);

/// The OTA state of the running image, for the `status` line.
///
/// - `pending`: on probation, has not confirmed itself yet
/// - `valid`: confirmed; the bootloader will keep booting it. Also the state
///   after a USB flash: the bootloader fills the erased `otadata` in for
///   `ota_0` on first boot
/// - `aborted` / `invalid`: this image came back from a rolled-back attempt
/// - `undefined`: booted without an OTA verdict for this slot
/// - `unknown`: the OTA data could not be read at all
#[must_use]
pub fn state_str() -> &'static str {
    match OTA_STATE.load(Ordering::Relaxed) {
        STATE_UNDEFINED => "undefined",
        STATE_PENDING => "pending",
        STATE_VALID => "valid",
        STATE_INVALID => "invalid",
        STATE_ABORTED => "aborted",
        _ => "unknown",
    }
}

/// Is a reboot into a freshly received image imminent?
#[must_use]
pub fn rebooting() -> bool {
    REBOOTING.load(Ordering::Relaxed)
}

const fn state_code(state: OtaImageState) -> u8 {
    match state {
        OtaImageState::New | OtaImageState::PendingVerify => STATE_PENDING,
        OtaImageState::Valid => STATE_VALID,
        OtaImageState::Invalid => STATE_INVALID,
        OtaImageState::Aborted => STATE_ABORTED,
        OtaImageState::Undefined => STATE_UNDEFINED,
    }
}

// ---------------------------------------------------------------------------
// Planned-reboot marker (RTC fast memory)
// ---------------------------------------------------------------------------

/// `[magic, reason, uptime_ms, crc]` in RTC fast memory, which survives a
/// software reset (but not a power cut).
///
/// Its only job is to say "the master on this bus a moment ago was us", which
/// lets the next boot skip the 3 s silence check and keep the gap in the poll
/// stream under a second. It is honoured only together with a software reset
/// reason, so a power-on - where the stock controller may have been plugged
/// back in - always does the full check.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut BOOT_MARKER: [u32; 4] = [0; 4];

/// "WRB1": Wfi ReBoot marker, rev 1.
const MARKER_MAGIC: u32 = u32::from_le_bytes(*b"WRB1");

/// Why the firmware reset itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A new image was received and activated.
    Ota,
    /// The running image did not confirm itself in time.
    Probation,
}

impl Reason {
    const fn code(self) -> u32 {
        match self {
            Self::Ota => 1,
            Self::Probation => 2,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Ota => "ota",
            Self::Probation => "probation",
        }
    }

    const fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Ota),
            2 => Some(Self::Probation),
            _ => None,
        }
    }
}

/// CRC over the first three words, so a reset in the middle of writing the
/// marker (or uninitialised RTC memory) cannot be mistaken for a valid one.
fn marker_crc(words: &[u32; 4]) -> u32 {
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&words[0].to_le_bytes());
    bytes[4..8].copy_from_slice(&words[1].to_le_bytes());
    bytes[8..12].copy_from_slice(&words[2].to_le_bytes());
    u32::from(sniffer::crc16(&bytes))
}

/// Leave the marker and reset. The caller has already said what it is doing
/// in the capture log.
pub fn planned_reset(reason: Reason) -> ! {
    let uptime = u32::try_from(Instant::now().as_millis()).unwrap_or(u32::MAX);
    let mut words = [MARKER_MAGIC, reason.code(), uptime, 0];
    words[3] = marker_crc(&words);
    // Volatile and all at once: this memory is not re-initialised on the next
    // boot, so a partially written marker would be read as real.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(BOOT_MARKER), words) };
    esp_hal::system::software_reset()
}

/// Read the marker and clear it, so it is honoured exactly once.
fn take_marker() -> Option<Reason> {
    let words = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(BOOT_MARKER)) };
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(BOOT_MARKER), [0u32; 4]) };

    if words[0] != MARKER_MAGIC || words[3] != marker_crc(&words) {
        return None;
    }
    Reason::from_code(words[1])
}

/// Did the firmware reset itself, as opposed to the power dropping, a
/// watchdog firing or a panic?
fn after_software_reset() -> bool {
    matches!(
        esp_hal::system::reset_reason(),
        Some(SocResetReason::CoreSw | SocResetReason::Cpu0Sw)
    )
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// What the boot-time OTA checks found.
#[derive(Debug, Clone, Copy)]
pub struct Boot {
    /// This boot follows a reboot the firmware itself initiated seconds ago,
    /// so the bus cannot have another master on it: the bus task may go
    /// straight to master mode.
    pub skip_silence_check: bool,
    /// The running image has not been confirmed yet.
    pub probation: bool,
}

/// Read the planned-reboot marker and the running image's OTA state.
///
/// Call once from `main`, after [`crate::settings::init`] (it needs the flash
/// lock) and before the bus task starts (it decides the silence check).
pub async fn boot_check() -> Boot {
    let marker = take_marker();
    let software = after_software_reset();
    let skip_silence_check = marker.is_some() && software;

    if let Some(reason) = marker {
        if software {
            crate::publish_note(format_args!(
                "boot after planned reset ({}): skipping the silence check once",
                reason.as_str()
            ));
        } else {
            // The marker survived a reset we did not cause: honour nothing.
            crate::publish_note(format_args!(
                "boot: planned-reset marker ({}) but the reset was not a software reset; \
                 full silence check",
                reason.as_str()
            ));
        }
    }

    let state = with_ota(|updater| updater.current_ota_state()).await;
    let probation = match state {
        Ok(state) => {
            OTA_STATE.store(state_code(state), Ordering::Relaxed);
            matches!(state, OtaImageState::New | OtaImageState::PendingVerify)
        }
        Err(reason) => {
            // No OTA data (a board flashed with the old single-app table, or
            // a read error): nothing is on probation and nothing can be
            // confirmed. Updates will refuse for the same reason.
            crate::publish_note(format_args!("ota: no image state ({reason})"));
            OTA_STATE.store(STATE_UNKNOWN, Ordering::Relaxed);
            false
        }
    };

    Boot {
        skip_silence_check,
        probation,
    }
}

/// Start the probation watchdog, if this image is on probation.
pub fn start(spawner: &Spawner, boot: Boot) {
    if boot.probation {
        spawner.spawn(probation_task().unwrap());
    }
}

/// Start the receiver. Called from [`crate::net`], which owns the stack.
pub fn serve(spawner: &Spawner, stack: Stack<'static>) {
    spawner.spawn(ota_task(stack).unwrap());
}

// ---------------------------------------------------------------------------
// Flash helpers
// ---------------------------------------------------------------------------

/// Run `f` with an [`OtaUpdater`] over the partition table.
///
/// The 3 KB partition-table buffer is a local here rather than a field of the
/// OTA task, because every caller needs it only for the length of one flash
/// operation.
async fn with_ota<R>(
    f: impl FnOnce(&mut OtaUpdater<'_, '_>) -> Result<R, esp_bootloader_esp_idf::partitions::Error>,
) -> Result<R, &'static str> {
    crate::settings::with_flash(|flash| {
        let mut table = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let mut updater =
            OtaUpdater::new(flash, &mut table).map_err(|_| "no two app slots and otadata")?;
        f(&mut updater).map_err(|_| "ota data access failed")
    })
    .await?
}

/// Set the running image's state in `otadata`.
async fn set_state(state: OtaImageState) -> Result<(), &'static str> {
    with_ota(|updater| updater.set_current_ota_state(state)).await
}

/// The app slot an update would be written to.
async fn next_slot() -> Result<(PartitionEntry, AppPartitionSubType), &'static str> {
    crate::settings::with_flash(|flash| {
        let mut table = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        // Which slot is next is the updater's decision (it refuses to pick
        // the one that is running); the entry itself then comes from a plain
        // table lookup, because the entry outlives the borrow of the table.
        let subtype = {
            let mut updater =
                OtaUpdater::new(flash, &mut table).map_err(|_| "no two app slots and otadata")?;
            updater.next_partition().map_err(|_| "no free app slot")?.1
        };
        let table = partitions::read_partition_table(flash, &mut table)
            .map_err(|_| "unreadable partition table")?;
        let entry = table
            .find_partition(PartitionType::App(subtype))
            .map_err(|_| "unreadable partition table")?
            .ok_or("the next app slot is not in the table")?;
        Ok((entry, subtype))
    })
    .await?
}

/// Human name of an app slot, for the log lines.
const fn slot_name(subtype: AppPartitionSubType) -> &'static str {
    match subtype {
        AppPartitionSubType::Ota0 => "ota_0",
        AppPartitionSubType::Ota1 => "ota_1",
        AppPartitionSubType::Factory => "factory",
        _ => "ota_n",
    }
}

// ---------------------------------------------------------------------------
// The receiver
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn ota_task(stack: Stack<'static>) {
    let mut socket = TcpSocket::new(
        stack,
        OTA_RX.init([0; OTA_RX_BUF]),
        OTA_TX.init([0; OTA_TX_BUF]),
    );
    // No keep-alive: a push is a short, busy conversation. The timeout is
    // what releases the socket if the host vanishes mid-image.
    socket.set_timeout(Some(CHUNK_TIMEOUT));

    // One socket, so exactly one update can be in flight: a second connection
    // while one runs is refused by the stack, which is the intended answer.
    loop {
        if socket.accept(OTA_PORT).await.is_err() {
            socket.abort();
            let _ = socket.flush().await;
            Timer::after(Duration::from_millis(200)).await;
            continue;
        }

        crate::publish_note(format_args!("ota: push started on port {OTA_PORT}"));
        let accepted = receive(&mut socket).await;
        let _ = socket.flush().await;
        socket.abort();
        let _ = socket.flush().await;

        if accepted {
            // From here the only way out is the reboot: the new image is
            // selected in otadata and the one running is a boot away from
            // being the standby.
            master::fail_queued_for_reboot();
            Timer::after(REBOOT_DELAY).await;
            planned_reset(Reason::Ota);
        }
    }
}

/// Receive one image. `true` once the new slot is activated and the caller
/// should reboot into it.
async fn receive(socket: &mut TcpSocket<'static>) -> bool {
    // 1. The header, before anything else is even looked up.
    let mut buf = [0u8; HEADER_LEN];
    if let Err(reason) = read_exact(socket, &mut buf, HEADER_TIMEOUT).await {
        refuse(socket, reason).await;
        return false;
    }

    // 2. The slot to write, which is also the size the image has to fit.
    let (entry, subtype) = match next_slot().await {
        Ok(slot) => slot,
        Err(reason) => {
            refuse(socket, reason).await;
            return false;
        }
    };

    // 3. Magic, version, target, signature, size. Nothing has been erased.
    let header = match Header::accept(&buf, TARGET, &PUBLIC_KEY, entry.len()) {
        Ok(header) => header,
        Err(reject) => {
            refuse(socket, reject.as_str()).await;
            return false;
        }
    };

    let image_len = header.image_len;
    say(
        socket,
        format_args!(
            "ok header len={image_len} fw={} slot={}",
            header.fw_version_name(),
            slot_name(subtype)
        ),
    )
    .await;

    // 4. The image itself, one sector at a time: read, hash, write.
    let mut hasher = Sha256::new();
    let mut head = [0u8; 16];
    let mut written = 0u32;
    let mut next_progress = PROGRESS_EVERY;
    let mut sector = [0u8; SECTOR as usize];

    while written < image_len {
        let want = (image_len - written).min(SECTOR) as usize;
        if let Err(reason) = read_exact(socket, &mut sector[..want], CHUNK_TIMEOUT).await {
            refuse(socket, reason).await;
            return false;
        }
        hasher.update(&sector[..want]);
        if written == 0 {
            let seen = want.min(head.len());
            head[..seen].copy_from_slice(&sector[..seen]);
        }

        // esp-storage's write is read-modify-erase-write per sector, so this
        // one call both erases and programs. One sector at a time keeps the
        // CPU stall around 40 ms - short enough for the UART FIFO to hold the
        // heat pump's reply while it happens.
        let offset = written;
        let result = crate::settings::with_flash(|flash| {
            entry
                .as_flash_region(flash)
                .write(offset, &sector[..want])
                .map_err(|_| "flash write failed")
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(reason)) | Err(reason) => {
                refuse(socket, reason).await;
                return false;
            }
        }

        written += want as u32;
        if written >= next_progress || written == image_len {
            say(socket, format_args!("progress {written}/{image_len}")).await;
            next_progress = written.saturating_add(PROGRESS_EVERY);
        }
    }

    // 5. The image is on flash but nothing points at it yet: verify.
    let digest = hasher.finalize();
    if digest.as_slice() != header.image_sha {
        refuse(socket, "sha mismatch: the image does not match its header").await;
        return false;
    }
    if let Err(reason) = check_esp_image(&head) {
        refuse(socket, reason).await;
        return false;
    }

    // 6. Point otadata at it, as unverified. The bootloader turns `New` into
    // `PendingVerify` and rolls back if nothing confirms it.
    if let Err(reason) = with_ota(|updater| {
        updater.activate_next_partition()?;
        updater.set_current_ota_state(OtaImageState::New)
    })
    .await
    {
        refuse(socket, reason).await;
        return false;
    }

    REBOOTING.store(true, Ordering::Relaxed);
    OTA_STATE.store(STATE_PENDING, Ordering::Relaxed);
    say(socket, format_args!("ok image sha verified")).await;
    say(
        socket,
        format_args!(
            "ok rebooting into {} on probation ({} s to confirm)",
            slot_name(subtype),
            PROBATION_LIMIT.as_secs()
        ),
    )
    .await;
    true
}

/// Does this look like an ESP32-C6 application image?
///
/// The last check before `otadata` moves: a correctly signed image for the
/// wrong chip would be a brick that only a USB cable can undo.
fn check_esp_image(head: &[u8; 16]) -> Result<(), &'static str> {
    /// First byte of an ESP-IDF application image.
    const IMAGE_MAGIC: u8 = 0xe9;
    /// `chip_id` in `esp_image_header_t`, ESP32-C6.
    const CHIP_ID_ESP32C6: u16 = 13;

    if head[0] != IMAGE_MAGIC {
        return Err("not an ESP application image (magic)");
    }
    let chip_id = u16::from_le_bytes([head[12], head[13]]);
    if chip_id != CHIP_ID_ESP32C6 {
        return Err("image is for another chip");
    }
    Ok(())
}

/// Read exactly `out.len()` bytes, or give up.
async fn read_exact(
    socket: &mut TcpSocket<'static>,
    out: &mut [u8],
    timeout: Duration,
) -> Result<(), &'static str> {
    match select(socket.read_exact(out), Timer::after(timeout)).await {
        Either::First(Ok(())) => Ok(()),
        Either::First(Err(_)) => Err("connection closed before the image was complete"),
        Either::Second(()) => Err("timed out waiting for image data"),
    }
}

/// Say one line to the pushing host AND put it in the capture log, so an
/// update is as visible to the capture files as it is to the operator.
async fn say(socket: &mut TcpSocket<'static>, args: core::fmt::Arguments<'_>) {
    crate::publish_note(format_args!("ota: {args}"));
    let mut line = Line::new();
    if line.write_fmt(args).is_err() || line.push_str("\r\n").is_err() {
        return;
    }
    let _ = socket.write_all(line.as_bytes()).await;
    let _ = socket.flush().await;
}

/// Refuse the push with one reason, in the same shape as the line interface's
/// errors.
async fn refuse(socket: &mut TcpSocket<'static>, reason: &str) {
    say(socket, format_args!("err {reason}")).await;
}

// ---------------------------------------------------------------------------
// Probation
// ---------------------------------------------------------------------------

/// Watch the new image do its job, and confirm it - or hand control back to
/// the bootloader, which rolls back.
///
/// A crash, a panic or a wedged bus task does not need this task at all: the
/// hardware watchdog (`crate::poll`) or the panic handler resets the chip,
/// the slot is still `PendingVerify`, and the bootloader aborts it on the
/// next boot. This task only catches the quieter failure - an image that runs
/// but cannot do the job.
#[embassy_executor::task]
async fn probation_task() {
    let deadline = Instant::now() + PROBATION_LIMIT;
    let frames_at_start = crate::frames();
    let uart_errors_at_start = crate::uart_errors();
    let mut broker_pending = mqtt::effective().is_some();

    crate::publish_note(format_args!(
        "ota: this image is on probation, {} s to confirm \
         (mode {}, broker {})",
        PROBATION_LIMIT.as_secs(),
        master::mode().as_str(),
        if broker_pending { "configured" } else { "none" },
    ));

    let mut snapshots = master::SNAPSHOT.receiver();
    let mut last: Option<master::Counters> = None;
    let mut clean = 0u32;

    loop {
        if broker_pending && mqtt::connection() == mqtt::Connection::Online {
            broker_pending = false;
        }

        if let Some(snapshot) = master::SNAPSHOT.try_get() {
            let counters = snapshot.counters;
            if let Some(previous) = last {
                let polls = (counters.status_ok + counters.settings_ok)
                    .saturating_sub(previous.status_ok + previous.settings_ok);
                if counters.timeouts > previous.timeouts {
                    clean = 0;
                } else {
                    clean = clean.saturating_add(polls);
                }
            }
            last = Some(counters);
        }

        if let Some(why) = confirmed(deadline, clean, frames_at_start, uart_errors_at_start) {
            match set_state(OtaImageState::Valid).await {
                Ok(()) => {
                    OTA_STATE.store(STATE_VALID, Ordering::Relaxed);
                    crate::publish_note(format_args!("ota: image confirmed ({why})"));
                }
                // Could not write otadata: say so and stay pending. The next
                // reset then rolls back, which is the safe direction.
                Err(reason) => crate::publish_note(format_args!(
                    "ota: image works ({why}) but otadata could not be written: {reason}"
                )),
            }
            return;
        }

        if Instant::now() >= deadline {
            crate::publish_note(format_args!(
                "ota: not confirmed within {} s (mode {}, link {}, clean polls {}, \
                 frames {}, broker {}): resetting, the bootloader will roll back",
                PROBATION_LIMIT.as_secs(),
                master::mode().as_str(),
                master::SNAPSHOT
                    .try_get()
                    .map_or("down", |s| s.link.as_str()),
                clean,
                crate::frames().saturating_sub(frames_at_start),
                if broker_pending {
                    "not connected"
                } else {
                    "ok"
                },
            ));
            // Let that line reach the capture daemon and any console before
            // the network goes down with us.
            Timer::after(REBOOT_DELAY).await;
            planned_reset(Reason::Probation);
        }

        // A snapshot comes after every poll in master mode; the tick is for
        // listen mode, where nothing may be published for minutes.
        match snapshots.as_mut() {
            Some(receiver) => {
                let _ = select(receiver.changed(), Timer::after(PROBATION_TICK)).await;
            }
            None => Timer::after(PROBATION_TICK).await,
        }
    }
}

/// Has the running image proved it can do its job? `Some(why)` says what
/// convinced it, for the log.
///
/// Deliberately not "did anything at all happen": the image has to show the
/// one thing it exists for - a working bus - and, if a broker is configured,
/// that it can reach Home Assistant.
fn confirmed(
    deadline: Instant,
    clean: u32,
    frames_at_start: u32,
    uart_errors_at_start: u32,
) -> Option<&'static str> {
    if mqtt::effective().is_some() && mqtt::connection() != mqtt::Connection::Online {
        return None;
    }

    match master::mode() {
        master::OpMode::Master => {
            let link_up = master::SNAPSHOT
                .try_get()
                .is_some_and(|snapshot| snapshot.link == master::LinkState::Up);
            (link_up && clean >= CLEAN_POLLS).then_some("link up, 10 clean polls")
        }
        master::OpMode::Listen => {
            if crate::frames().saturating_sub(frames_at_start) >= LISTEN_FRAMES {
                return Some("10 frames seen in listen mode");
            }
            // A bus nobody is driving says nothing at all, which is not a
            // fault: a quiet UART that produced no errors is the best this
            // mode can offer.
            let waited = deadline
                .checked_duration_since(Instant::now())
                .map_or(PROBATION_LIMIT, |left| PROBATION_LIMIT - left);
            (waited >= LISTEN_QUIET && crate::uart_errors() == uart_errors_at_start)
                .then_some("quiet bus, no UART error in 30 s")
        }
    }
}
