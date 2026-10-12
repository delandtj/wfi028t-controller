//! Manual compressor frequency: the task around [`hp_model::manual::Manual`]
//! (ADR 0007).
//!
//! MQTT (`wfi028t/manual_hz/set`) and the console (`set mfreq`) only ever
//! call [`request`]; nothing else queues [`Command::SetManualFreq`]. The task
//! steps the manager once per fresh settings block in master mode and queues
//! whatever it answers, so the lease, the safety rules and the write rate
//! limit hold whoever asked.
//!
//! Before the first step after boot nothing is requested, so a manual
//! frequency left set by a previous run is cleared on the first settings
//! poll (reason `boot`).

use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::Instant;

use hp_model::manual::{Manual, Reason};
use hp_model::Command;

use crate::master::{self, OpMode};

static MANAGER: Mutex<CriticalSectionRawMutex, RefCell<Manual>> =
    Mutex::new(RefCell::new(Manual::new()));

/// What the outside sees of the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// Frequency asked for, while its lease runs.
    pub requested_hz: Option<u8>,
    /// Why manual frequency is not running; `-` when it is, or was switched
    /// off on request.
    pub reason: Reason,
    /// Seconds left on the lease.
    pub lease_s: u32,
}

/// Ask for `Some(hz)` (30-90) for `lease_ms`, or for off. Takes effect at
/// the next settings poll (within 1 s) if the rules allow it.
///
/// # Errors
///
/// `hz` out of range; the previous request stands.
pub fn request(hz: Option<u8>, lease_ms: u64) -> Result<(), &'static str> {
    let now = Instant::now().as_millis();
    MANAGER.lock(|m| m.borrow_mut().request(hz, now, lease_ms))
}

/// Snapshot of the manager for MQTT and the status line.
#[must_use]
pub fn view() -> View {
    let now = Instant::now().as_millis();
    MANAGER.lock(|m| {
        let m = m.borrow();
        View {
            requested_hz: m.requested_hz(),
            reason: m.reason(),
            lease_s: m.lease_s(now),
        }
    })
}

/// Start the manager task. Needs the bus task, nothing else.
pub fn start(spawner: &Spawner) {
    spawner.spawn(manual_task().unwrap());
}

#[embassy_executor::task]
async fn manual_task() {
    let Some(mut snapshots) = master::SNAPSHOT.receiver() else {
        // Every slot taken (see master::SUBSCRIBERS). Without the task
        // nothing sets manual frequency, but a leftover is not cleared
        // either; say so.
        crate::publish_note(format_args!(
            "manual frequency: no snapshot receiver free, manager not running"
        ));
        return;
    };
    let mut last_settings_ms = 0u64;

    loop {
        let snapshot = snapshots.changed().await;
        if snapshot.mode != OpMode::Master || snapshot.settings_ms == last_settings_ms {
            continue;
        }
        let Some(settings) = snapshot.settings else {
            continue;
        };
        last_settings_ms = snapshot.settings_ms;

        let now = Instant::now().as_millis();
        let (command, before, after) = MANAGER.lock(|m| {
            let mut m = m.borrow_mut();
            let before = m.reason();
            let command = m.step(now, &settings, snapshot.status.as_ref());
            (command, before, m.reason())
        });

        if after != before {
            crate::publish_note(format_args!(
                "manual frequency: reason {} -> {}",
                before.as_str(),
                after.as_str()
            ));
        }
        if let Some(command) = command {
            if !master::submit(command) {
                // Not lost: the manager queues it again after its settle
                // time, from the state the heat pump reports then.
                crate::publish_note(format_args!("manual frequency: command queue full"));
            }
        }
    }
}

/// End manual frequency now, without waiting for the task: for `mode listen`,
/// after which nobody would clear it. Returns the command to queue if the
/// heat pump currently holds it.
pub fn release_for_listen() -> Option<Command> {
    MANAGER.lock(|m| {
        let _ = m.borrow_mut().request(None, 0, 0);
    });
    let settings = master::SNAPSHOT.try_get()?.settings?;
    settings
        .manual_freq()
        .then_some(Command::SetManualFreq(None))
}
