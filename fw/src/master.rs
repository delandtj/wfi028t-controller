//! What the bus master looks like from outside: its state, its queue and its
//! mode. The task that drives the wire is [`crate::poll`].
//!
//! Everything a consumer of the heat pump needs is here, so a new task (MQTT
//! next) only has to touch this module:
//!
//! | Interface | Type |
//! |---|---|
//! | [`SNAPSHOT`] | `Watch<CriticalSectionRawMutex, Snapshot, 6>` out |
//! | [`COMMANDS`] | `Channel<CriticalSectionRawMutex, hp_model::Command, 4>` in |
//! | [`OUTCOMES`] | `Watch<CriticalSectionRawMutex, CommandReport, 6>` out |
//! | [`mode`] / [`request_mode`] | listen or master, persisted in flash |
//!
//! A snapshot is published after every poll and on every mode, link or command
//! change, so a subscriber can sit on `changed()` and never poll. Commands are
//! validated by `hp_model` before they reach the bus, applied at most one per
//! 1 s cycle, and every one of them produces exactly one [`CommandReport`].
//!
//! The mode is the safety interlock from the ADR: [`OpMode::Listen`] is the
//! default and never transmits, and [`OpMode::Master`] can only be entered
//! through [`request_mode`], which listens for another master first.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU8, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_sync::watch::Watch;
use embassy_time::{Duration, Timer};

use hp_model::{settings::Mode, Command, Rejected, Settings, Status};
use modbus_sniffer_core::Line;

/// How long "listen first and make sure nobody else is master" listens.
pub const SILENCE_CHECK: Duration = Duration::from_millis(3_000);

// ---------------------------------------------------------------------------
// Public state
// ---------------------------------------------------------------------------

/// What the firmware is doing on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpMode {
    /// Receive only, never transmit: exactly the sniffer. The default.
    Listen,
    /// Poll and write like the stock controller.
    Master,
}

impl OpMode {
    /// Lower-case name, as used by the `mode` command and the status line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Listen => "listen",
            Self::Master => "master",
        }
    }

    /// Parse the argument of the `mode` command.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("listen") {
            Some(Self::Listen)
        } else if text.eq_ignore_ascii_case("master") {
            Some(Self::Master)
        } else {
            None
        }
    }

    /// On-flash and atomic representation.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Listen => 0,
            Self::Master => 1,
        }
    }

    /// Inverse of [`code`](Self::code); anything else is `None`.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Listen),
            1 => Some(Self::Master),
            _ => None,
        }
    }
}

/// Whether the heat pump is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// At least one answer, and fewer than [`LINK_DOWN_AFTER`] timeouts since.
    Up,
    /// [`LINK_DOWN_AFTER`] consecutive timeouts, or nothing heard yet.
    Down,
}

impl LinkState {
    /// `"up"` or `"down"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// Bus-level counters, carried in every [`Snapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Requests sent (reads and writes).
    pub requests: u32,
    /// Status blocks decoded.
    pub status_ok: u32,
    /// Settings blocks decoded.
    pub settings_ok: u32,
    /// Requests that got no usable answer in time.
    pub timeouts: u32,
    /// Answers that arrived but did not parse as the reply we asked for.
    pub bad_responses: u32,
    /// Frames recognised as the echo of our own transmission.
    pub echoes: u32,
    /// Frames that looked like somebody else's request.
    pub foreign: u32,
    /// Whole-block writes sent.
    pub writes: u32,
    /// Commands that did not end in [`CommandOutcome::Applied`].
    pub write_failures: u32,
}

/// Everything the rest of the firmware needs to know about the heat pump.
///
/// Published on [`SNAPSHOT`] after every poll and on every mode, link or
/// command change.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    /// Last decoded status block.
    pub status: Option<Status>,
    /// Last decoded settings block.
    pub settings: Option<Settings>,
    /// Uptime in ms when `status` was decoded.
    pub status_ms: u64,
    /// Uptime in ms when `settings` was decoded.
    pub settings_ms: u64,
    /// Uptime in ms when this snapshot was published.
    pub updated_ms: u64,
    /// What the firmware is doing on the bus.
    pub mode: OpMode,
    /// Whether the heat pump is answering.
    pub link: LinkState,
    /// Bus-level counters since boot.
    pub counters: Counters,
}

impl Snapshot {
    /// Age of the settings block at `now_ms`, saturating. [`u32::MAX`] when no
    /// block has ever been decoded.
    #[must_use]
    pub fn settings_age_ms(&self, now_ms: u64) -> u32 {
        if self.settings.is_none() {
            return u32::MAX;
        }
        u32::try_from(now_ms.saturating_sub(self.settings_ms)).unwrap_or(u32::MAX)
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Written, acked and verified in the readback.
    Applied,
    /// The model refused the value; nothing was sent.
    Rejected(Rejected),
    /// No settings block fresh enough to write back (link down, or stale).
    NoFreshSettings,
    /// None of the three writes was acked.
    NoAck,
    /// Acked, but the next settings poll did not show the change.
    ReadbackMismatch,
    /// The firmware is in listen mode and will not transmit.
    NotMaster,
    /// An update was accepted and the firmware is rebooting into it, so this
    /// command will never be sent. Re-issue it after the reboot; the heat
    /// pump keeps whatever it was set to (ADR 0002).
    Rebooting,
}

impl CommandOutcome {
    /// Short tag for logs and replies.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "ok",
            Self::Rejected(_) => "rejected",
            Self::NoFreshSettings => "no-fresh-settings",
            Self::NoAck => "no-ack",
            Self::ReadbackMismatch => "readback-mismatch",
            Self::NotMaster => "not-master",
            Self::Rebooting => "rebooting",
        }
    }
}

/// One command and what became of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandReport {
    /// The command as it was received.
    pub command: Command,
    /// How it ended.
    pub outcome: CommandOutcome,
}

/// Receivers [`SNAPSHOT`] and [`OUTCOMES`] can hand out. The MQTT task takes
/// one of each at boot and keeps them; the line interface takes a short-lived
/// one per `set` command (one per transport at worst, and there are three of
/// those since ADR 0002 added the console); the OTA probation task takes one
/// for its first two minutes; the rest is headroom.
const SUBSCRIBERS: usize = 8;

/// Latest decoded state. Subscribe with `SNAPSHOT.receiver()`.
pub static SNAPSHOT: Watch<CriticalSectionRawMutex, Snapshot, SUBSCRIBERS> = Watch::new();

/// A [`SNAPSHOT`] subscription, as held for the lifetime of a task.
pub type SnapshotReceiver =
    embassy_sync::watch::Receiver<'static, CriticalSectionRawMutex, Snapshot, SUBSCRIBERS>;

/// An [`OUTCOMES`] subscription, as held for the lifetime of a task.
pub type OutcomeReceiver =
    embassy_sync::watch::Receiver<'static, CriticalSectionRawMutex, CommandReport, SUBSCRIBERS>;

/// Commands into the bus master. At most one is applied per cycle.
pub static COMMANDS: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();

/// Result of the last command taken. Subscribe before sending the command.
pub static OUTCOMES: Watch<CriticalSectionRawMutex, CommandReport, SUBSCRIBERS> = Watch::new();

/// Effective mode, as an [`OpMode::code`].
static MODE: AtomicU8 = AtomicU8::new(0);

/// A mode the line interface asked for.
pub(crate) static MODE_REQUEST: Signal<CriticalSectionRawMutex, OpMode> = Signal::new();

/// The bus task's answer to [`MODE_REQUEST`].
pub(crate) static MODE_RESULT: Signal<CriticalSectionRawMutex, Result<OpMode, &'static str>> =
    Signal::new();

/// What the firmware is doing on the bus right now.
#[must_use]
pub fn mode() -> OpMode {
    OpMode::from_code(MODE.load(Ordering::Relaxed)).unwrap_or(OpMode::Listen)
}

pub(crate) fn set_mode(new: OpMode) {
    MODE.store(new.code(), Ordering::Relaxed);
    crate::led::set_master(new == OpMode::Master);
}

/// Ask the bus task to change mode, and wait for its answer.
///
/// Switching to master costs [`SILENCE_CHECK`], because that is how long it
/// listens for another master first. The `Err` payload is protocol-visible
/// text.
pub async fn request_mode(wanted: OpMode) -> Result<OpMode, &'static str> {
    if cfg!(feature = "selftest") {
        return Err("this build has no bus task");
    }
    MODE_RESULT.reset();
    MODE_REQUEST.signal(wanted);
    let timeout = SILENCE_CHECK + Duration::from_secs(3);
    match embassy_futures::select::select(MODE_RESULT.wait(), Timer::after(timeout)).await {
        embassy_futures::select::Either::First(result) => result,
        embassy_futures::select::Either::Second(()) => Err("bus task did not answer"),
    }
}

/// Queue a command for the bus master, without waiting for the outcome.
///
/// `false` means the queue is full and the command was dropped.
pub fn submit(command: Command) -> bool {
    COMMANDS.try_send(command).is_ok()
}

/// Fail everything in the queue because the firmware is about to reboot into
/// a freshly received image ([`crate::ota`]).
///
/// The bus task is not involved: it may be mid-slot, and in a few hundred
/// milliseconds it will not exist. Every waiter gets a real outcome
/// ([`CommandOutcome::Rebooting`]) instead of its six-second timeout, and the
/// capture log says what became of each command.
pub fn fail_queued_for_reboot() {
    while let Ok(command) = COMMANDS.try_receive() {
        let report = CommandReport {
            command,
            outcome: CommandOutcome::Rebooting,
        };
        let mut line = Line::new();
        if write_report(&mut line, &report).is_ok() {
            crate::publish_note(format_args!("command {line}"));
        }
        OUTCOMES.sender().send(report);
    }
}

// ---------------------------------------------------------------------------
// Formatting (shared with the line interface)
// ---------------------------------------------------------------------------

/// Write a command the way the `set` command spells it: `power on`, `p01 34`.
pub fn write_command(out: &mut Line, command: &Command) -> core::fmt::Result {
    match command {
        Command::SetPower(on) => write!(out, "power {}", on_off(*on)),
        Command::SetBoost(on) => write!(out, "boost {}", on_off(*on)),
        Command::SetStopAtTarget(on) => write!(out, "p05 {}", on_off(*on)),
        Command::SetMode(Mode::Heat) => write!(out, "mode heat"),
        Command::SetMode(Mode::Cool) => write!(out, "mode cool"),
        Command::SetMode(Mode::Auto) => write!(out, "mode auto"),
        Command::SetMode(Mode::Other(raw)) => write!(out, "mode 0x{raw:04x}"),
        Command::SetHeatSetpoint(v) => write!(out, "p01 {v}"),
        Command::SetCoolSetpoint(v) => write!(out, "p02 {v}"),
        Command::SetAutoSetpoint(v) => write!(out, "p03 {v}"),
        Command::SetHysteresis(v) => write!(out, "p04 {v}"),
    }
}

const fn on_off(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// Write `<command> -> <outcome>`, with the reason spelled out for a rejection.
pub fn write_report(out: &mut Line, report: &CommandReport) -> core::fmt::Result {
    write_command(out, &report.command)?;
    write!(out, " -> {}", report.outcome.as_str())?;
    match report.outcome {
        CommandOutcome::Rejected(Rejected::OutOfRange {
            value, min, max, ..
        }) => write!(out, " ({value} outside {min}..={max})"),
        CommandOutcome::Rejected(Rejected::UnsupportedMode(raw)) => {
            write!(out, " (mode 0x{raw:04x} not supported)")
        }
        _ => Ok(()),
    }
}
