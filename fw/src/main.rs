//! Replacement controller for the WFI-028T/035T pool heat pump, on an
//! ESP32-C6-DevKitC-1 (ESP32-C6-WROOM-1).
//!
//! This firmware is the sniffer firmware plus a bus master. It still streams
//! every frame it sees to two outputs at once:
//!
//! - the chip's built-in USB-Serial-JTAG peripheral, which enumerates as
//!   /dev/ttyACM0 on the port labelled "USB" (not the "UART" port, which is the
//!   CP2102N bridge), and
//! - a TCP server on port 4000 over WiFi, for unattended multi-day captures.
//!
//! Both carry the same line protocol and both accept the same commands (`bus`,
//! `status`, `note`, `mode`, `set`). Frames the controller sends itself go into
//! the same stream, tagged `dir=tx` (see [`publish_tx_frame`]).
//!
//! Wiring against the sniffer: UART1 RX stays on GPIO4, UART1 **TX is GPIO5**.
//! The RS-485 module switches direction from its TX input, so that one wire is
//! the whole hardware delta.
//!
//! What runs where:
//!
//! | Module | Job |
//! |---|---|
//! | [`bus`] | UART1: t3.5 framing, transmit, echo detection |
//! | [`poll`] | the master task: 1 s poll, whole-block writes, mode switch |
//! | [`master`] | state, command queue and mode for everyone else |
//! | [`cmd`] | the line interface on both transports |
//! | [`net`] | WiFi, DHCP, TCP 4000 |
//! | [`mqtt`] | MQTT 3.1.1 client and Home Assistant discovery |
//! | [`led`] | status LED |
//! | [`settings`] | bus configuration, mode and MQTT broker in flash |
//!
//! Polling does not wait for WiFi: the bus task is spawned before the radio and
//! keeps running whatever the network does (ADR, "Fails safe"). MQTT is a
//! consumer of [`master`] like any other, so a broker that is down, slow or
//! not configured at all changes nothing on the bus.
//!
//! Framing, CRC, line formatting, the ring and the sniffer commands live in
//! `modbus-sniffer-core`; the register model lives in `hp-model`.

#![no_std]
#![no_main]
// A `selftest` image leaves the bus task out, so the mode, link and command
// machinery it drives has no caller in that build. The default build is the
// one that has to be dead-code clean.
#![cfg_attr(feature = "selftest", allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read as _, Write as _};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_hal::Async;

// The panic handler comes from esp-backtrace, configured with the `println`
// feature; esp-println is pinned to its `jtag-serial` backend (see Cargo.toml),
// so a panic message and backtrace land on the SAME /dev/ttyACM0 the analyzer
// output goes to - exactly where the user is already looking.
//
// This coexists with the async UsbSerialJtag driver below because esp-println's
// serial-JTAG backend touches only the USB_DEVICE FIFO/CONF registers directly:
// it never takes the peripheral singleton, it polls instead of using the
// interrupt, and it gives up after a bounded spin if no host is draining the
// FIFO (so a panic can never turn into a silent hang on an unattached board).
// By the time it runs, the driver's own writes are over anyway - esp-backtrace
// prints and then parks the core with interrupts off.
use esp_backtrace as _;

use modbus_sniffer_core as sniffer;
use sniffer::{BusConfig, BusFormat, Chunk, Line, Marker, SnifferBus};

mod cmd;
mod led;
mod master;
mod mqtt;
mod net;
mod settings;

// UART1 and the task that drives it exist only in a real build: a `selftest`
// image has no bus access at all, which is the point of it.
#[cfg(not(feature = "selftest"))]
mod bus;
#[cfg(not(feature = "selftest"))]
mod poll;

pub use cmd::CommandBuffer;

// espflash needs an ESP-IDF application descriptor in the image, otherwise the
// second-stage bootloader rejects it.
esp_bootloader_esp_idf::esp_app_desc!();

// ---------------------------------------------------------------------------
// Build-time configuration
// ---------------------------------------------------------------------------

/// WiFi credentials come from the environment at build time and are never
/// committed:
///
/// ```sh
/// SNIFFER_WIFI_SSID=myap SNIFFER_WIFI_PASS=secret cargo run --release
/// ```
///
/// The variable names are the sniffer's, so one `.cargo/config.toml` serves
/// both projects. With no SSID the build still succeeds and WiFi is simply not
/// started.
const WIFI_SSID: Option<&str> = option_env!("SNIFFER_WIFI_SSID");
const WIFI_PASS: Option<&str> = option_env!("SNIFFER_WIFI_PASS");

/// Name this device reports in its hello line. Distinct from the sniffer's
/// `sniffer-esp32c6`, so a capture log says which box produced it.
pub const DEVICE_NAME: &str = "wfi-controller-esp32c6";

/// What the firmware runs with when flash holds no valid record: the heat pump
/// bus as captured (`docs/register-map.md`, "Bus").
///
/// Deliberately not `sniffer::DEFAULT_BUS`, which is 9600 8E1 - the setting the
/// sniffer was last pointed at.
pub const DEFAULT_BUS: BusConfig = BusConfig {
    baud: 9_600,
    format: BusFormat::N1,
};

/// Heap for esp-radio: the WiFi driver's buffers and the RTOS task stacks it
/// creates. The radio needs a few tens of KB; the rest of the firmware never
/// allocates.
const HEAP_SIZE: usize = 96 * 1024;

/// The token that marks our own transmissions in the capture stream.
///
/// It sits right after `RX`, where the sniffer's host parser
/// (`../stm32-modbus-sniffer/tools/src/device.rs`) ignores it: `parse_frame`
/// only looks at `slave=`, `func=`, `data=`, `crc=`, `computed=` and the final
/// `OK`/`BAD_CRC`. So the capture daemon, the compactor and the analyzer treat
/// a frame we sent exactly like a frame we heard - which is what keeps
/// request/response pairing and poll de-duplication working now that we are the
/// one asking - while a human (and a future parser) can still see the
/// direction. Appending the token after `OK` would have broken `crc_ok`, and a
/// line starting with `TX ` would have become `DeviceLine::Unknown`.
const TX_TAG: &str = "dir=tx ";

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// The one ring every output reads from. ~64 KB of line storage plus a slot
/// table; all-zero, so it lands in .bss.
static LINES: SnifferBus = SnifferBus::new();

/// The bus parameters currently in force.
static BUS: BlockingMutex<CriticalSectionRawMutex, core::cell::Cell<BusConfig>> =
    BlockingMutex::new(core::cell::Cell::new(DEFAULT_BUS));

/// Tells the bus task to re-apply the UART configuration.
static BUS_CHANGED: Signal<CriticalSectionRawMutex, BusConfig> = Signal::new();

/// Command replies on their way back to the USB host that asked.
static USB_REPLIES: Channel<CriticalSectionRawMutex, Line, 2> = Channel::new();

/// Frames received since boot, including short and bad-CRC ones. Frames we
/// sent are not counted here; [`master::Counters`] has those.
static FRAMES: AtomicU32 = AtomicU32::new(0);
/// Frames of 4 bytes or more whose CRC did not check out. Short frames are not
/// counted here - they have no CRC to be wrong.
static BAD_CRC: AtomicU32 = AtomicU32::new(0);
/// UART receive errors (framing, parity, noise, overrun).
static UART_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Lines the TCP consumer had to skip, plus lines the ring refused outright:
/// real gaps in the server-side capture.
static DROPPED_LINES: AtomicU32 = AtomicU32::new(0);
/// Lines the USB consumer had to skip. Kept separate because it grows forever
/// when no USB host is reading (board powered from a charger in the field).
static USB_DROPPED_LINES: AtomicU32 = AtomicU32::new(0);

/// True while the stack holds a DHCP lease.
pub static WIFI_UP: AtomicBool = AtomicBool::new(false);
/// Current IPv4 address in big-endian byte order, or 0 for "none".
pub static WIFI_IP: AtomicU32 = AtomicU32::new(0);
/// Sentinel for "no RSSI reading available".
pub const RSSI_UNKNOWN: i32 = i32::MIN;
/// Last RSSI sample in dBm, or [`RSSI_UNKNOWN`].
pub static WIFI_RSSI: AtomicI32 = AtomicI32::new(RSSI_UNKNOWN);

/// The shared line ring.
pub fn line_bus() -> &'static SnifferBus {
    &LINES
}

/// The bus parameters currently in force.
pub fn current_bus() -> BusConfig {
    BUS.lock(|bus| bus.get())
}

/// Record the bus parameters now in force.
pub fn set_bus(bus: BusConfig) {
    BUS.lock(|current| current.set(bus));
}

/// Ask the bus task to re-apply the UART configuration.
pub fn request_bus_change(bus: BusConfig) {
    BUS_CHANGED.signal(bus);
}

/// Take a pending bus change, if the `bus` command left one.
pub fn take_bus_change() -> Option<BusConfig> {
    BUS_CHANGED.try_take()
}

/// Frames received since boot.
pub fn frames() -> u32 {
    FRAMES.load(Ordering::Relaxed)
}

/// Received frames with a bad CRC.
pub fn bad_crc() -> u32 {
    BAD_CRC.load(Ordering::Relaxed)
}

/// UART receive errors.
pub fn uart_errors() -> u32 {
    UART_ERRORS.load(Ordering::Relaxed)
}

/// Count one UART receive error.
pub fn count_uart_error() {
    UART_ERRORS.fetch_add(1, Ordering::Relaxed);
}

/// Lines lost to the TCP consumer or refused by the ring.
pub fn dropped_lines() -> u32 {
    DROPPED_LINES.load(Ordering::Relaxed)
}

/// Lines the USB consumer skipped.
pub fn usb_dropped_lines() -> u32 {
    USB_DROPPED_LINES.load(Ordering::Relaxed)
}

/// Count lines the TCP consumer had to skip past.
pub fn count_dropped_lines(missed: u64) {
    DROPPED_LINES.fetch_add(u32::try_from(missed).unwrap_or(u32::MAX), Ordering::Relaxed);
}

/// Publish one ready-made line (including its CRLF) into the ring.
pub fn publish_text(line: &str) {
    if !LINES.publish(line.as_bytes()) {
        DROPPED_LINES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Publish a `# note [secs.ms] <text>` line, the sniffer's own note format.
///
/// Used for mode changes, link transitions and command outcomes, so they land
/// in the capture log in temporal order with the frames they explain. The
/// sniffer's host parser files these under notes.
pub fn publish_note(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write as _;

    let now = Instant::now().as_millis();
    let mut line = Line::new();
    if write!(line, "# note [{}.{:03}] ", now / 1000, now % 1000).is_err()
        || line.write_fmt(args).is_err()
        || line.push_str("\r\n").is_err()
    {
        sniffer::DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    publish_text(&line);
}

/// Format one received frame and publish it, with any pending frame-loss marker
/// ahead of it so the loss shows up in temporal order.
pub fn publish_frame(buf: &[u8], timestamp_ms: u64) {
    let frame = sniffer::make_frame(buf, timestamp_ms);
    FRAMES.fetch_add(1, Ordering::Relaxed);
    if frame.data.len() >= 4 && !frame.crc_ok {
        BAD_CRC.fetch_add(1, Ordering::Relaxed);
    }

    // Blink before the formatting, so a frame that is too long to format still
    // shows up on the LED. Setting a flag is all this does.
    led::flash(!frame.crc_ok);

    let mut line = Line::new();
    if sniffer::format_frame(&frame, &mut line).is_err() {
        // Line buffer too small for this frame: count it as lost rather than
        // emitting a truncated, lying line. The marker goes out with the next
        // frame that does format.
        sniffer::DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }

    flush_dropped_marker();
    publish_text(&line);
}

/// Format one frame **we sent** and publish it, tagged [`TX_TAG`].
///
/// Everything else about the line is the sniffer's own frame format, produced
/// by the same formatter, so the only difference a host sees is one extra
/// token.
pub fn publish_tx_frame(buf: &[u8], timestamp_ms: u64) {
    let frame = sniffer::make_frame(buf, timestamp_ms);
    led::flash(false);

    let mut line = Line::new();
    if sniffer::format_frame(&frame, &mut line).is_err() {
        sniffer::DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Splice the tag in after "RX ". Short frames (under 4 bytes) have no such
    // marker and we never send one; publish those unchanged rather than lose
    // them.
    let tagged = match line.find("RX ") {
        Some(at) => {
            let mut out = Line::new();
            let head = at + 3;
            if out.push_str(&line[..head]).is_err()
                || out.push_str(TX_TAG).is_err()
                || out.push_str(&line[head..]).is_err()
            {
                sniffer::DROPPED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            out
        }
        None => line,
    };

    flush_dropped_marker();
    publish_text(&tagged);
}

/// Emit the "frames were lost here" marker if any frames were dropped.
fn flush_dropped_marker() {
    let dropped = sniffer::take_dropped();
    if dropped > 0 {
        let mut marker = Marker::new();
        if sniffer::format_dropped_marker(dropped, &mut marker).is_ok() {
            publish_text(&marker);
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[esp_hal::main]
async fn main(spawner: Spawner) {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // esp-radio allocates; the allocator has to exist before the radio starts.
    esp_alloc::heap_allocator!(size: HEAP_SIZE);

    // esp-rtos owns the embassy-time driver, so it has to be running before
    // anything awaits a Timer, and before esp-radio initialises (the radio
    // driver needs the scheduler). The #[main] macro has already put us inside
    // its thread-mode executor; starting the scheduler from the main task is
    // the documented order.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Bus settings and operating mode from flash (or the defaults) before
    // anything uses them.
    let (bus_config, persisted_mode) = settings::init(peripherals.FLASH).await;
    set_bus(bus_config);

    // The status LED first, so the board shows something while the radio comes
    // up. It owns RMT TX channel 0 and GPIO8 from here on.
    led::start(&spawner, peripherals.RMT, peripherals.GPIO8);

    // The bus task before WiFi: polling the heat pump must not wait for the
    // network, and must survive it going away.
    #[cfg(not(feature = "selftest"))]
    {
        // TIMG1's watchdog, not TIMG0's: TIMG0 timer0 is the embassy-time
        // driver. esp_hal::init leaves all watchdogs disabled; the bus task
        // configures and feeds this one.
        let timg1 = TimerGroup::new(peripherals.TIMG1);
        poll::start(
            &spawner,
            peripherals.UART1,
            peripherals.GPIO4,
            peripherals.GPIO5,
            timg1.wdt,
            bus_config,
            persisted_mode,
        );
    }
    #[cfg(feature = "selftest")]
    {
        let _ = persisted_mode;
        spawner.spawn(selftest_task().unwrap());
    }

    let (usb_rx, usb_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();
    spawner.spawn(usb_tx_task(usb_tx).unwrap());
    spawner.spawn(usb_rx_task(usb_rx).unwrap());

    let wifi_available = match WIFI_SSID {
        Some(ssid) => net::start(&spawner, peripherals.WIFI, ssid, WIFI_PASS.unwrap_or("")),
        None => {
            publish_text("# wifi disabled (no SNIFFER_WIFI_SSID at build time)\r\n");
            false
        }
    };
    led::set_wifi_available(wifi_available);
}

// ---------------------------------------------------------------------------
// USB-Serial-JTAG consumer
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn usb_tx_task(mut tx: UsbSerialJtagTx<'static, Async>) {
    let mut cursor = 0u64;
    let mut buf = [0u8; sniffer::MAX_LINE_LEN];
    let mut marker = Marker::new();

    loop {
        // If no host is draining the port the writes below stall. That only
        // holds up THIS cursor: the bus task and the TCP consumer carry on, and
        // the ring eventually overtakes us, which shows up as a
        // "[DROPPED n lines]" marker once a host shows up again.
        let out = select(
            LINES.next(sniffer::CONSUMER_USB, &mut cursor, &mut buf),
            USB_REPLIES.receive(),
        )
        .await;

        let written = match out {
            Either::First(Chunk::Line(len)) => tx.write_all(&buf[..len]).await,
            Either::First(Chunk::Dropped(missed)) => {
                USB_DROPPED_LINES
                    .fetch_add(u32::try_from(missed).unwrap_or(u32::MAX), Ordering::Relaxed);
                if sniffer::format_dropped_lines(missed, &mut marker).is_ok() {
                    tx.write_all(marker.as_bytes()).await
                } else {
                    Ok(())
                }
            }
            // `next` only returns Empty if it is told not to wait.
            Either::First(Chunk::Empty) => Ok(()),
            Either::Second(reply) => tx.write_all(reply.as_bytes()).await,
        };
        if written.is_ok() {
            let _ = tx.flush().await;
        }
    }
}

#[embassy_executor::task]
async fn usb_rx_task(mut rx: UsbSerialJtagRx<'static, Async>) {
    let mut command = CommandBuffer::new();
    let mut chunk = [0u8; 64];

    loop {
        match rx.read(&mut chunk).await {
            Ok(n) => {
                for &byte in &chunk[..n] {
                    if let Some(reply) = command.feed(byte).await {
                        USB_REPLIES.send(reply).await;
                    }
                }
            }
            // A receive error here is not fatal; back off so a stuck peripheral
            // cannot spin the CPU.
            Err(_) => Timer::after(Duration::from_millis(100)).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test task (feature = "selftest")
//
// Injects canned Modbus RTU frames through the same producers the bus task
// uses, so the output paths and both frame line formats (received and `dir=tx`)
// can be verified without a live RS-485 bus. UART1 is not initialised at all in
// this build, so it cannot transmit; WiFi, the TCP server and the commands all
// still work, and `mode master` answers "this build has no bus task".
// ---------------------------------------------------------------------------

#[cfg(feature = "selftest")]
#[embassy_executor::task]
async fn selftest_task() {
    use hp_model::rtu;

    loop {
        // 1) Our own status-block request, as the master would send it.
        publish_tx_frame(&rtu::read_status_request(), Instant::now().as_millis());
        Timer::after(Duration::from_millis(150)).await;

        // 2) Slave response: 20 bytes of register data.
        publish_frame(
            &sniffer::build_frame(&[
                0x01, 0x03, 0x14, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00,
                0x06, 0x00, 0x07, 0x00, 0x08, 0x00, 0x09, 0x00, 0x0a,
            ]),
            Instant::now().as_millis(),
        );
        Timer::after(Duration::from_millis(350)).await;

        // 3) Our own settings-block request.
        publish_tx_frame(&rtu::read_settings_request(), Instant::now().as_millis());
        Timer::after(Duration::from_millis(150)).await;

        // 4) Deliberately corrupted frame -> exercises the BAD_CRC path.
        let mut bad = sniffer::build_frame(&[0x02, 0x06, 0x00, 0x10, 0x12, 0x34]);
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        publish_frame(&bad, Instant::now().as_millis());
        Timer::after(Duration::from_millis(350)).await;
    }
}
