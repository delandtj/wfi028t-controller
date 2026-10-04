//! The polling task: the half of the bus master that actually drives the wire.
//!
//! Types, statics and the mode switch live in [`crate::master`]; this module is
//! the task, and it is the only code in the firmware that transmits. It is not
//! compiled into a `selftest` build, which therefore cannot transmit at all.
//!
//! # Timing
//!
//! The capture (see `docs/register-map.md`, "Controller behaviour") shows the
//! stock controller on a 1 s cycle built from two 500 ms slots:
//!
//! | Offset | Frame |
//! |---|---|
//! | +0 ms | `01 03 0000 003f` - status block, answered in ~150 ms |
//! | +500 ms | `01 03 003f 0043` - settings block, answered in ~160 ms |
//!
//! This task keeps that grid with absolute deadlines, so a slow response
//! cannot make the cycle drift. A slot costs ~200 ms of the 500 ms, and the
//! rest is spent listening - which is how a second master gets noticed.
//!
//! **Writes sit between the polls, they do not replace them.** A settings
//! change is three whole-block writes 500 ms apart; each one goes out in the
//! idle part of a slot, right after that slot's read has been answered:
//!
//! | Slot | Request | Write |
//! |---|---|---|
//! | n (settings) | `03 003f 0043` | write 1, immediately after the read |
//! | n+1 (status) | `03 0000 003f` | write 2 |
//! | n+2 (settings) | `03 003f 0043` | write 3 |
//! | n+4 (settings) | `03 003f 0043` | readback verified here |
//!
//! The 500 ms spacing of the writes therefore comes from the slot grid itself,
//! and the heat pump never goes longer without a poll than it does in the
//! capture - which matters because a silent master is the suspected cause of
//! E09 (ADR risk "E09 when the master stops"). Writes only start right after a
//! settings read, so the block we send back is always the one we just read
//! ([`hp_model::write_allowed`] re-checks the age at the moment of sending).
//!
//! # Two masters
//!
//! The top risk in the ADR is transmitting while the stock controller is still
//! plugged in. Three defences:
//!
//! 1. Entering master mode (by command, or because the persisted mode says so)
//!    listens for [`SILENCE_CHECK`] first and refuses if any valid frame
//!    appears - with nobody transmitting, the heat pump says nothing at all, so
//!    one CRC-good frame means another master.
//! 2. While in master mode every received frame is classified; a frame that is
//!    neither our echo nor an answer from the heat pump drops this task back to
//!    listen mode at once and says so in the capture stream.
//! 3. Listen mode never transmits. It is the default and the fallback.
//!
//! The watchdog is fed here, at every slot boundary in both modes: a wedged bus
//! task becomes a reboot (about a second of silence) instead of a heat pump
//! left without a master.

use core::fmt::Write as _;

use embassy_time::{Duration, Instant};
use esp_hal::timer::timg::{MwdtStage, Wdt};

use hp_model::{rtu, Command, Settings, Status};
use modbus_sniffer_core as sniffer;
use sniffer::{BusConfig, Line};

use crate::bus::{self, BusUart, Rx, RxPin, TxPin};
use crate::master::{
    mode, set_mode, write_command, write_report, CommandOutcome, CommandReport, Counters,
    LinkState, OpMode, Snapshot, COMMANDS, MODE_REQUEST, MODE_RESULT, OUTCOMES, SILENCE_CHECK,
    SNAPSHOT,
};

// ---------------------------------------------------------------------------
// Tuning
// ---------------------------------------------------------------------------

/// Half a cycle: one read, then idle. Two slots make the 1 s poll.
const SLOT: Duration = Duration::from_millis(500);

/// How long the heat pump gets to answer a request. It answers a read in about
/// 150 ms and a write in about 24 ms.
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Consecutive request timeouts before the link counts as down.
const LINK_DOWN_AFTER: u32 = 3;

/// Whole-block writes per command, like the stock controller.
const WRITE_REPEATS: u8 = 3;

/// Listen-mode slice: how often the mode request, the bus setting and the
/// watchdog are serviced while nothing is being transmitted.
const LISTEN_SLICE: Duration = Duration::from_millis(250);

/// Hardware watchdog timeout. Generous against the 500 ms slot it is fed from,
/// tight against the (unmeasured) E09 tolerance.
const WATCHDOG: esp_hal::time::Duration = esp_hal::time::Duration::from_secs(8);

/// Readback registers reported per note line, so one surprise cannot overrun
/// the line buffer.
const MAX_REPORTED_REGS: usize = 6;

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// How the bus task should come up.
#[derive(Debug, Clone, Copy)]
pub struct Startup {
    /// The mode flash remembers.
    pub mode: OpMode,
    /// Skip the [`SILENCE_CHECK`] once, because this boot follows a reboot
    /// the firmware itself initiated moments ago ([`crate::ota::boot_check`])
    /// and the only master on the bus then was us.
    pub skip_silence_check: bool,
}

/// Start the bus task. It owns UART1 and both bus pins from here on.
pub fn start(
    spawner: &embassy_executor::Spawner,
    uart: esp_hal::peripherals::UART1<'static>,
    rx_pin: RxPin,
    tx_pin: TxPin,
    wdt: Wdt<esp_hal::peripherals::TIMG1<'static>>,
    initial_bus: BusConfig,
    startup: Startup,
) {
    spawner.spawn(bus_task(uart, rx_pin, tx_pin, wdt, initial_bus, startup).unwrap());
}

#[embassy_executor::task]
async fn bus_task(
    uart: esp_hal::peripherals::UART1<'static>,
    rx_pin: RxPin,
    tx_pin: TxPin,
    mut wdt: Wdt<esp_hal::peripherals::TIMG1<'static>>,
    initial_bus: BusConfig,
    startup: Startup,
) {
    let mut bus = BusUart::new(uart, rx_pin, tx_pin, initial_bus);
    let mut st = State::new();

    // Stage 0 resets the system. Fed at every slot boundary below, in both
    // modes, so a wedged bus task is a reboot rather than a silent bus.
    wdt.set_timeout(MwdtStage::Stage0, WATCHDOG);
    wdt.enable();

    // Listen mode until proven otherwise, even if flash says master: the
    // silence check below is what grants the right to transmit.
    set_mode(OpMode::Listen);
    st.publish();

    if startup.mode == OpMode::Master {
        if startup.skip_silence_check {
            // A planned reboot: we were the master on this bus a few hundred
            // milliseconds ago, so there is nothing to listen for and three
            // seconds of silence is exactly what the heat pump must not get
            // (ADR 0002, component 5). Only a software reset with a valid RTC
            // marker gets here; a power-on never does.
            crate::publish_note(format_args!(
                "boot mode master after a planned reboot: skipping the {} ms silence check",
                SILENCE_CHECK.as_millis()
            ));
            st.reset_cycle();
            set_mode(OpMode::Master);
            st.publish();
        } else {
            crate::publish_note(format_args!(
                "boot mode master: listening {} ms for another master",
                SILENCE_CHECK.as_millis()
            ));
            let _ = engage_master(&mut bus, &mut st, &mut wdt).await;
        }
    }

    loop {
        wdt.feed();
        apply_bus_change(&mut bus);

        if let Some(wanted) = MODE_REQUEST.try_take() {
            let result = match wanted {
                OpMode::Listen => {
                    if mode() == OpMode::Master {
                        set_mode(OpMode::Listen);
                        bus.clear_echo();
                        st.abandon_write(CommandOutcome::NotMaster);
                        crate::publish_note(format_args!("mode listen"));
                        st.publish();
                    }
                    Ok(OpMode::Listen)
                }
                OpMode::Master => {
                    if mode() == OpMode::Master {
                        Ok(OpMode::Master)
                    } else {
                        engage_master(&mut bus, &mut st, &mut wdt).await
                    }
                }
            };
            MODE_RESULT.signal(result);
            continue;
        }

        match mode() {
            OpMode::Listen => listen(&mut bus, &mut st, LISTEN_SLICE).await,
            OpMode::Master => slot(&mut bus, &mut st).await,
        }
    }
}

/// Re-apply a `bus` command's configuration, if one came in.
fn apply_bus_change(bus: &mut BusUart) {
    let Some(requested) = crate::take_bus_change() else {
        return;
    };
    if bus.apply(requested) {
        let mut line = Line::new();
        if sniffer::format_bus_line(requested, &mut line).is_ok() {
            crate::publish_text(&line);
        }
    } else {
        // The hardware refused it. Put the old setting back where it is
        // visible - in every consumer's log and in `status` - rather than
        // reporting a configuration the UART is not actually using.
        crate::set_bus(bus.config());
        let mut line = Line::new();
        if sniffer::format_bus_line(bus.config(), &mut line).is_ok() {
            crate::publish_text(&line);
        }
    }
}

// ---------------------------------------------------------------------------
// Listen mode
// ---------------------------------------------------------------------------

/// Receive (and publish) frames for `slice`, like the sniffer does.
///
/// Commands cannot be served here, so any that arrive are answered
/// [`CommandOutcome::NotMaster`] rather than piling up until master mode.
async fn listen(bus: &mut BusUart, st: &mut State, slice: Duration) {
    while let Ok(command) = COMMANDS.try_receive() {
        st.report(CommandReport {
            command,
            outcome: CommandOutcome::NotMaster,
        });
    }

    let deadline = Instant::now() + slice;
    loop {
        match bus.recv(deadline).await {
            Rx::Frame => {
                st.counters.echoes += u32::from(bus.take_echo());
                crate::publish_frame(bus.frame(), Instant::now().as_millis());
            }
            Rx::Error => st.uart_error(bus),
            Rx::Deadline => return,
        }
    }
}

/// Listen for [`SILENCE_CHECK`], then switch to master mode if the bus stayed
/// quiet.
///
/// Nothing but another master can make a frame appear here: the heat pump is a
/// slave and never speaks unasked. Short and bad-CRC frames are counted but do
/// not block the switch, so a noisy or half-connected RX line cannot lock the
/// controller out of master mode for good.
async fn engage_master(
    bus: &mut BusUart,
    st: &mut State,
    wdt: &mut Wdt<esp_hal::peripherals::TIMG1<'static>>,
) -> Result<OpMode, &'static str> {
    let until = Instant::now() + SILENCE_CHECK;
    let mut quiet = true;

    while Instant::now() < until {
        wdt.feed();
        let slice = Instant::now() + LISTEN_SLICE;
        match bus.recv(slice.min(until)).await {
            Rx::Frame => {
                crate::publish_frame(bus.frame(), Instant::now().as_millis());
                if bus::crc_ok(bus.frame()) {
                    st.counters.foreign += 1;
                    quiet = false;
                    break;
                }
            }
            Rx::Error => st.uart_error(bus),
            Rx::Deadline => {}
        }
    }

    if !quiet {
        crate::publish_note(format_args!(
            "mode master refused: another master is active"
        ));
        st.publish();
        return Err("another master is active");
    }

    st.reset_cycle();
    set_mode(OpMode::Master);
    crate::publish_note(format_args!(
        "mode master engaged: polling status +0 ms, settings +{} ms",
        SLOT.as_millis()
    ));
    st.publish();
    Ok(OpMode::Master)
}

/// Leave master mode because somebody else is transmitting.
fn drop_to_listen(bus: &mut BusUart, st: &mut State) {
    set_mode(OpMode::Listen);
    bus.clear_echo();
    st.abandon_write(CommandOutcome::NotMaster);
    st.counters.foreign += 1;
    let frame = bus.frame();
    let head = &frame[..frame.len().min(8)];
    let mut line = Line::new();
    let _ = write!(line, "foreign request frame");
    for byte in head {
        let _ = write!(line, " {byte:02x}");
    }
    let _ = write!(line, " -> dropping to listen mode");
    crate::publish_note(format_args!("{line}"));
    st.publish();
}

// ---------------------------------------------------------------------------
// Master mode: one slot
// ---------------------------------------------------------------------------

/// Run one 500 ms slot: one read, any write that is due, then listen out the
/// rest of the slot.
async fn slot(bus: &mut BusUart, st: &mut State) {
    let now = Instant::now();
    if st.slot_start + SLOT < now {
        // First slot after a mode change, or a slot that overran. Re-align
        // rather than firing a burst of catch-up requests.
        st.slot_start = now;
    }
    let slot_start = st.slot_start;
    let settings_slot = !st.slot.is_multiple_of(2);
    if let Some(write) = st.write.as_mut() {
        write.sent_this_slot = false;
    }

    // 1. The read.
    let request = if settings_slot {
        rtu::read_settings_request()
    } else {
        rtu::read_status_request()
    };
    bus.send(&request).await;
    st.counters.requests += 1;

    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    let kind = if settings_slot {
        Reply::Settings
    } else {
        Reply::Status
    };
    let answered = match wait_for(bus, st, kind, deadline).await {
        Answer::Ok => true,
        Answer::Timeout => {
            st.timeout(bus);
            false
        }
        Answer::Foreign => {
            drop_to_listen(bus, st);
            return;
        }
    };

    // 2. A write, in the idle part of the slot, right after the read. Only a
    // settings read that just succeeded opens the write window.
    if let Answer::Foreign = write_step(bus, st, settings_slot && answered).await {
        drop_to_listen(bus, st);
        return;
    }

    st.publish();

    // 3. Listen out the rest of the slot, so a second master is noticed even
    // between our own requests.
    st.slot = st.slot.wrapping_add(1);
    st.slot_start = slot_start + SLOT;
    if idle_until(bus, st, st.slot_start).await == Answer::Foreign {
        drop_to_listen(bus, st);
    }
}

/// Which reply a request is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    Status,
    Settings,
    WriteAck,
}

/// What came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Ok,
    Timeout,
    Foreign,
}

/// Wait for the reply to the request we just sent, publishing everything that
/// arrives on the way.
async fn wait_for(bus: &mut BusUart, st: &mut State, kind: Reply, deadline: Instant) -> Answer {
    loop {
        match bus.recv(deadline).await {
            Rx::Deadline => return Answer::Timeout,
            Rx::Error => {
                st.uart_error(bus);
                continue;
            }
            Rx::Frame => {}
        }

        if bus.take_echo() {
            st.counters.echoes += 1;
            continue;
        }
        crate::publish_frame(bus.frame(), Instant::now().as_millis());

        let frame = bus.frame();
        if !bus::crc_ok(frame) {
            // Noise, or a mangled echo from the RX FIFO overrunning while we
            // transmitted. Never a reason to give up master mode.
            st.counters.bad_responses += 1;
            continue;
        }
        if is_foreign_request(frame) {
            return Answer::Foreign;
        }

        let parsed = match kind {
            Reply::Status => match Status::from_response(frame) {
                Ok(status) => {
                    st.status = Some(status);
                    st.status_ms = Instant::now().as_millis();
                    st.counters.status_ok += 1;
                    true
                }
                Err(_) => false,
            },
            Reply::Settings => match Settings::from_response(frame) {
                Ok(settings) => {
                    st.settings = Some(settings);
                    st.settings_ms = Instant::now().as_millis();
                    st.counters.settings_ok += 1;
                    true
                }
                Err(_) => false,
            },
            Reply::WriteAck => rtu::parse_settings_write_ack(frame).is_ok(),
        };

        if parsed {
            // The answer is in, so any echo of our request has either come and
            // gone or will never come. Dropping the expectation means a foreign
            // frame that happens to be identical to our last request is seen
            // for what it is.
            bus.clear_echo();
            st.answered();
            return Answer::Ok;
        }
        // A well-formed frame from the heat pump that is not the reply we
        // asked for: a late answer to the previous request, most likely.
        st.counters.bad_responses += 1;
    }
}

/// Listen until `deadline` without transmitting, inside master mode.
async fn idle_until(bus: &mut BusUart, st: &mut State, deadline: Instant) -> Answer {
    loop {
        match bus.recv(deadline).await {
            Rx::Deadline => return Answer::Ok,
            Rx::Error => {
                st.uart_error(bus);
                continue;
            }
            Rx::Frame => {}
        }
        if bus.take_echo() {
            st.counters.echoes += 1;
            continue;
        }
        crate::publish_frame(bus.frame(), Instant::now().as_millis());
        let frame = bus.frame();
        if !bus::crc_ok(frame) {
            st.counters.bad_responses += 1;
            continue;
        }
        if is_foreign_request(frame) {
            return Answer::Foreign;
        }
        // A stray but well-formed response from the heat pump (a late answer).
        st.counters.bad_responses += 1;
    }
}

/// Does this CRC-good frame look like somebody else's request?
///
/// Our own echo is filtered out before this is called, and the heat pump only
/// ever answers 0x03, 0x10 or an exception, so:
///
/// - a frame addressed to another slave means another master is polling it;
/// - an 8-byte 0x03 frame is a request (a 0x03 response is 5 + 2n bytes, which
///   is always odd);
/// - an 0x10 frame of 11 bytes or more is a request (the ack is 8 bytes);
/// - any other function code addressed to the heat pump (0x06 included - we
///   never send it) is somebody else's request;
/// - anything with the exception bit set is an answer, not a request.
fn is_foreign_request(frame: &[u8]) -> bool {
    if frame.len() < 4 {
        return false;
    }
    if frame[0] != rtu::SLAVE {
        return true;
    }
    let func = frame[1];
    if func & sniffer::EXCEPTION_FLAG != 0 {
        return false;
    }
    match func {
        rtu::FUNC_READ_HOLDING => frame.len() == rtu::REQUEST_LEN,
        rtu::FUNC_WRITE_MULTIPLE => frame.len() > rtu::WRITE_ACK_LEN + 2,
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Master mode: writes
// ---------------------------------------------------------------------------

/// The three-times whole-block write for one command.
#[derive(Debug, Clone, Copy)]
struct Write {
    command: Command,
    /// The block as read, before the command was applied.
    base: Settings,
    /// The block as sent.
    wanted: Settings,
    /// Writes sent so far.
    sent: u8,
    /// Acks received so far.
    acked: u8,
    /// All three are out; the next settings read decides the outcome.
    verifying: bool,
    /// One write per slot: set when this slot's write has gone out.
    sent_this_slot: bool,
    /// Settings slots that went by without an answer while verifying.
    verify_misses: u8,
}

/// Settings polls a verification waits for before giving up on it.
const VERIFY_ATTEMPTS: u8 = 3;

/// Take a command if one is waiting, start its write, or carry an existing
/// write one step forward. Runs in the idle part of a slot, after the read.
///
/// `fresh_settings` says whether THIS slot's settings read succeeded: a stale
/// block must never be written back, and must never be mistaken for a
/// readback.
async fn write_step(bus: &mut BusUart, st: &mut State, fresh_settings: bool) -> Answer {
    // A settings read just landed: that is the only moment a write may start,
    // and the only moment a readback can be checked.
    if fresh_settings {
        if let (Some(fresh), true) = (st.settings, st.write.is_some_and(|w| w.verifying)) {
            verify(st, fresh);
        }
        if st.write.is_none() {
            if let Some(fresh) = st.settings {
                start_write(st, fresh);
            }
        }
    } else if st.link == LinkState::Down {
        // The heat pump is not answering, so there is no fresh block to write
        // back: fail the queued commands instead of holding them forever.
        while let Ok(command) = COMMANDS.try_receive() {
            st.report(CommandReport {
                command,
                outcome: CommandOutcome::NoFreshSettings,
            });
        }
        // A verification that can never happen has to end too.
        if let Some(write) = st.write.as_mut() {
            if write.verifying {
                write.verify_misses += 1;
                if write.verify_misses >= VERIFY_ATTEMPTS {
                    let command = write.command;
                    st.write = None;
                    crate::publish_note(format_args!(
                        "readback not available: {VERIFY_ATTEMPTS} settings polls unanswered"
                    ));
                    st.report(CommandReport {
                        command,
                        outcome: CommandOutcome::ReadbackMismatch,
                    });
                }
            }
        }
    }

    let frame = {
        let Some(write) = st.write.as_mut() else {
            return Answer::Ok;
        };
        if write.verifying || write.sent >= WRITE_REPEATS || write.sent_this_slot {
            return Answer::Ok;
        }
        write.sent += 1;
        write.sent_this_slot = true;
        write.wanted.write_request()
    };
    st.counters.requests += 1;
    st.counters.writes += 1;
    bus.send(&frame).await;

    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    match wait_for(bus, st, Reply::WriteAck, deadline).await {
        Answer::Ok => {
            if let Some(write) = st.write.as_mut() {
                write.acked += 1;
            }
        }
        Answer::Timeout => st.timeout(bus),
        Answer::Foreign => return Answer::Foreign,
    }

    // Third write out: the next settings poll is the verdict.
    if let Some(write) = st.write.as_mut() {
        if write.sent >= WRITE_REPEATS {
            write.verifying = true;
            if write.acked == 0 {
                let report = CommandReport {
                    command: write.command,
                    outcome: CommandOutcome::NoAck,
                };
                st.write = None;
                st.report(report);
            }
        }
    }
    Answer::Ok
}

/// Take one queued command and build its write, right after a settings read.
fn start_write(st: &mut State, fresh: Settings) {
    let Ok(command) = COMMANDS.try_receive() else {
        return;
    };

    let age = u32::try_from(Instant::now().as_millis().saturating_sub(st.settings_ms))
        .unwrap_or(u32::MAX);
    if !hp_model::write_allowed(age) {
        st.report(CommandReport {
            command,
            outcome: CommandOutcome::NoFreshSettings,
        });
        return;
    }

    match fresh.apply(command) {
        Ok(wanted) => {
            let mut line = Line::new();
            let _ = write_command(&mut line, &command);
            crate::publish_note(format_args!(
                "write {line}: {} registers, {WRITE_REPEATS} times, settings age {age} ms",
                fresh.diff(&wanted).count()
            ));
            st.write = Some(Write {
                command,
                base: fresh,
                wanted,
                sent: 0,
                acked: 0,
                verifying: false,
                sent_this_slot: false,
                verify_misses: 0,
            });
        }
        Err(reason) => st.report(CommandReport {
            command,
            outcome: CommandOutcome::Rejected(reason),
        }),
    }
}

/// Compare the settings block that just came back with what we wrote.
fn verify(st: &mut State, got: Settings) {
    let Some(write) = st.write else {
        return;
    };
    st.write = None;

    // Only the registers the command was meant to change have to match.
    let mut mismatch = 0usize;
    let mut line = Line::new();
    for (addr, _, wanted) in write.base.diff(&write.wanted) {
        if got.reg(addr) != Some(wanted) {
            if mismatch < MAX_REPORTED_REGS {
                let actual = got.reg(addr).unwrap_or(0);
                let _ = write!(line, " 0x{addr:04x} want {wanted} got {actual}");
            }
            mismatch += 1;
        }
    }

    // Registers the heat pump changed by itself between our write and this
    // read. Not a failure - the stock controller has the same race - but worth
    // naming, because it is the open question in the ADR about whole-block
    // writes.
    // A register differs from what we sent and we did not touch it (we sent
    // back what we had just read): the heat pump moved it itself.
    let mut foreign_changes = Line::new();
    let mut foreign_count = 0usize;
    for (addr, sent, actual) in write.wanted.diff(&got) {
        if write.base.reg(addr) != Some(sent) {
            // One of the registers the command changed; the mismatch loop
            // above owns that case.
            continue;
        }
        if foreign_count < MAX_REPORTED_REGS {
            let _ = write!(foreign_changes, " 0x{addr:04x} {sent}->{actual}");
        }
        foreign_count += 1;
    }
    if foreign_count > 0 {
        crate::publish_note(format_args!(
            "readback: heat pump changed {foreign_count} other register(s):{foreign_changes}"
        ));
    }

    let outcome = if mismatch == 0 {
        CommandOutcome::Applied
    } else {
        crate::publish_note(format_args!(
            "readback mismatch on {mismatch} register(s):{line}"
        ));
        CommandOutcome::ReadbackMismatch
    };
    st.report(CommandReport {
        command: write.command,
        outcome,
    });
}

// ---------------------------------------------------------------------------
// Task state
// ---------------------------------------------------------------------------

struct State {
    status: Option<Status>,
    settings: Option<Settings>,
    status_ms: u64,
    settings_ms: u64,
    link: LinkState,
    consecutive_timeouts: u32,
    counters: Counters,
    write: Option<Write>,
    /// Slot number since master mode was engaged; even = status, odd =
    /// settings.
    slot: u64,
    /// When the current slot's request went (or should have gone) out.
    slot_start: Instant,
}

impl State {
    fn new() -> Self {
        Self {
            status: None,
            settings: None,
            status_ms: 0,
            settings_ms: 0,
            link: LinkState::Down,
            consecutive_timeouts: 0,
            counters: Counters::default(),
            write: None,
            slot: 0,
            slot_start: Instant::now(),
        }
    }

    /// Start the slot grid at "now", on entering master mode.
    fn reset_cycle(&mut self) {
        self.slot = 0;
        self.slot_start = Instant::now();
    }

    fn answered(&mut self) {
        self.consecutive_timeouts = 0;
        if self.link == LinkState::Down {
            self.link = LinkState::Up;
            crate::led::set_link_up(true);
            crate::publish_note(format_args!("link up"));
        }
    }

    fn timeout(&mut self, bus: &mut BusUart) {
        // Whatever we were waiting for is not coming; a late echo must not be
        // mistaken for one later on.
        bus.clear_echo();
        self.counters.timeouts += 1;
        self.consecutive_timeouts += 1;
        if self.link == LinkState::Up && self.consecutive_timeouts >= LINK_DOWN_AFTER {
            self.link = LinkState::Down;
            crate::led::set_link_up(false);
            crate::publish_note(format_args!(
                "link down after {} timeouts",
                self.consecutive_timeouts
            ));
        }
    }

    /// A UART receive error: count it, flash red, and log whatever fragment
    /// was in hand - the sniffer firmware's behaviour.
    ///
    /// The common fragment in master mode is our own echo cut short: a 143-byte
    /// settings write overruns the 128-byte RX FIFO while `write_async` has the
    /// task. That one is counted as an echo instead of being logged twice.
    fn uart_error(&mut self, bus: &mut BusUart) {
        crate::count_uart_error();
        crate::led::flash(true);
        if bus.take_echo() {
            self.counters.echoes += 1;
            return;
        }
        if !bus.frame().is_empty() {
            crate::publish_frame(bus.frame(), Instant::now().as_millis());
        }
    }

    /// Give up on the write in hand (mode change, foreign master).
    fn abandon_write(&mut self, outcome: CommandOutcome) {
        if let Some(write) = self.write.take() {
            self.report(CommandReport {
                command: write.command,
                outcome,
            });
        }
    }

    fn report(&mut self, report: CommandReport) {
        if report.outcome != CommandOutcome::Applied {
            self.counters.write_failures += 1;
        }
        let mut line = Line::new();
        if write_report(&mut line, &report).is_ok() {
            crate::publish_note(format_args!("command {line}"));
        }
        OUTCOMES.sender().send(report);
    }

    fn publish(&self) {
        SNAPSHOT.sender().send(Snapshot {
            status: self.status,
            settings: self.settings,
            status_ms: self.status_ms,
            settings_ms: self.settings_ms,
            updated_ms: Instant::now().as_millis(),
            mode: mode(),
            link: self.link,
            counters: self.counters,
        });
    }
}
