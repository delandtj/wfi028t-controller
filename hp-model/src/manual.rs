//! Manual compressor frequency: the lease and the safety rules (ADR 0007).
//!
//! The heat pump holds a manual frequency (0x003f bit 1 plus 0x0043) until
//! somebody clears it, across power cuts, and probably regardless of the
//! water temperature. [`Manual`] is the only thing allowed to ask for it. It
//! keeps what was requested and for how long, looks at every fresh pair of
//! blocks, and answers with at most one [`Command`] to queue:
//!
//! | Rule | When | Result |
//! |---|---|---|
//! | Lease | the request was not renewed in time | clear, [`Reason::Lease`] |
//! | Boot | bit 1 set at the first look, nothing requested | clear, [`Reason::Boot`] |
//! | Power | unit off | clear / refuse, [`Reason::Power`] |
//! | Mode | not heating | clear / refuse, [`Reason::Mode`] |
//! | Fault | [`Status::any_fault`] | clear / refuse, [`Reason::Fault`] |
//! | Setpoint | inlet water reached P01; latched until it is P04 below | clear / refuse, [`Reason::Setpoint`] |
//! | Rate | 0x0043 changes by less than [`MIN_STEP_HZ`], or within [`MIN_WRITE_INTERVAL_MS`] of the last write | hold |
//!
//! Clearing is never delayed by the rate rule; only changes of 0x0043 are.
//! Pure logic with a caller-supplied clock, so it is tested here on the host.

use crate::settings::{Command, Field, Mode, Settings};
use crate::status::Status;

/// Lease of a request from MQTT: HA resends every 30-60 s.
pub const LEASE_MQTT_MS: u64 = 180_000;
/// Lease of a request from the console: a person, not a loop.
pub const LEASE_CONSOLE_MS: u64 = 600_000;
/// Smallest change of 0x0043 worth a block write.
pub const MIN_STEP_HZ: u8 = 3;
/// Shortest time between two writes of 0x0043. With HA's loop at 30 s this
/// caps a full sunny day at ~200 frequency writes (each sent once, see
/// [`crate::Settings::write_repeats`]); at 60 s and three repeats the first
/// day of Managed made ~120 block writes an hour.
pub const MIN_WRITE_INTERVAL_MS: u64 = 180_000;
/// After queueing a command, wait this long for its readback before
/// queueing another. A write takes about 3.5 s including the readback.
pub const SETTLE_MS: u64 = 8_000;

/// Why manual frequency is not running although it may have been asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Running as requested, or switched off on request.
    None,
    /// The request was not renewed in time.
    Lease,
    /// Found set at the first look after boot, with nothing requested.
    Boot,
    /// The unit is off.
    Power,
    /// The unit is not in heating mode.
    Mode,
    /// A fault or protection flag is set.
    Fault,
    /// The inlet water reached the setpoint; latched until it drops by P04.
    Setpoint,
}

impl Reason {
    /// Lower-case tag for MQTT and the status line; `-` for [`Reason::None`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "-",
            Self::Lease => "lease",
            Self::Boot => "boot",
            Self::Power => "power",
            Self::Mode => "mode",
            Self::Fault => "fault",
            Self::Setpoint => "setpoint",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Request {
    hz: u8,
    until_ms: u64,
}

/// The manager's state. One instance, owned by the firmware task.
#[derive(Debug, Clone, Copy)]
pub struct Manual {
    request: Option<Request>,
    reason: Reason,
    latched: bool,
    looked: bool,
    last_hz_write_ms: Option<u64>,
    settle_until_ms: u64,
}

impl Default for Manual {
    fn default() -> Self {
        Self::new()
    }
}

impl Manual {
    /// Nothing requested, nothing seen yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            request: None,
            reason: Reason::None,
            latched: false,
            looked: false,
            last_hz_write_ms: None,
            settle_until_ms: 0,
        }
    }

    /// Ask for `Some(hz)` until `now_ms + lease_ms`, or for `None` (off).
    /// A repeat of the same value only renews the lease.
    ///
    /// # Errors
    ///
    /// `hz` outside the 30-90 Hz range; the request is left unchanged.
    pub fn request(
        &mut self,
        hz: Option<u8>,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<(), &'static str> {
        match hz {
            None => {
                self.request = None;
                self.reason = Reason::None;
            }
            Some(hz) => {
                let (min, max) = Field::ManualFreq.limits();
                if hz < min || hz > max {
                    return Err("outside 30..=90 Hz");
                }
                self.request = Some(Request {
                    hz,
                    until_ms: now_ms.saturating_add(lease_ms),
                });
            }
        }
        Ok(())
    }

    /// The frequency asked for, while its lease runs.
    #[must_use]
    pub fn requested_hz(&self) -> Option<u8> {
        self.request.map(|r| r.hz)
    }

    /// Seconds left on the lease, 0 when nothing is requested.
    #[must_use]
    pub fn lease_s(&self, now_ms: u64) -> u32 {
        self.request.map_or(0, |r| {
            u32::try_from(r.until_ms.saturating_sub(now_ms) / 1000).unwrap_or(u32::MAX)
        })
    }

    /// Why manual frequency is not (or no longer) running.
    #[must_use]
    pub const fn reason(&self) -> Reason {
        self.reason
    }

    /// Look at a fresh pair of blocks and return the command to queue, if
    /// any. Call it once per settings poll in master mode; `status` is the
    /// latest status block (`None` if there is none yet).
    pub fn step(
        &mut self,
        now_ms: u64,
        settings: &Settings,
        status: Option<&Status>,
    ) -> Option<Command> {
        let on = settings.manual_freq();
        let first_look = !self.looked;
        self.looked = true;

        if let Some(request) = self.request {
            if now_ms >= request.until_ms {
                self.request = None;
                self.reason = Reason::Lease;
            }
        }

        self.update_latch(settings, status, on);

        let block = if !settings.power() {
            Some(Reason::Power)
        } else if settings.mode() != Mode::Heat {
            Some(Reason::Mode)
        } else if status.is_some_and(Status::any_fault) {
            Some(Reason::Fault)
        } else if self.latched {
            Some(Reason::Setpoint)
        } else {
            None
        };

        let want = match (self.request, block) {
            (Some(request), None) => {
                self.reason = Reason::None;
                Some(request.hz)
            }
            (Some(_), Some(reason)) => {
                self.reason = reason;
                None
            }
            (None, _) => {
                if on && first_look {
                    self.reason = Reason::Boot;
                }
                None
            }
        };

        if now_ms < self.settle_until_ms {
            return None;
        }

        let command = match (on, want) {
            (true, None) => Some(Command::SetManualFreq(None)),
            (false, Some(hz)) => {
                self.last_hz_write_ms = Some(now_ms);
                Some(Command::SetManualFreq(Some(hz)))
            }
            (true, Some(hz)) => {
                let held = settings.manual_freq_hz();
                let step = u16::from(hz).abs_diff(held);
                let rested = self
                    .last_hz_write_ms
                    .is_none_or(|at| now_ms.saturating_sub(at) >= MIN_WRITE_INTERVAL_MS);
                if step >= u16::from(MIN_STEP_HZ) && rested {
                    self.last_hz_write_ms = Some(now_ms);
                    Some(Command::SetManualFreq(Some(hz)))
                } else {
                    None
                }
            }
            (false, None) => None,
        };
        if command.is_some() {
            self.settle_until_ms = now_ms.saturating_add(SETTLE_MS);
        }
        command
    }

    /// Set the setpoint latch when the inlet water reaches P01 while manual
    /// frequency is in use; release it once the water is P04 below P01.
    fn update_latch(&mut self, settings: &Settings, status: Option<&Status>, on: bool) {
        let Some(inlet) = status.and_then(Status::inlet_water) else {
            return;
        };
        let inlet = i32::from(inlet.tenths());
        let setpoint = i32::from(settings.heat_setpoint()) * 10;
        let hysteresis = i32::from(settings.hysteresis()) * 10;
        if inlet >= setpoint && (on || self.request.is_some()) {
            self.latched = true;
        } else if inlet <= setpoint - hysteresis {
            self.latched = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Manual, Reason, LEASE_MQTT_MS, MIN_WRITE_INTERVAL_MS, SETTLE_MS};
    use crate::rtu::{SETTINGS_START, STATUS_START};
    use crate::settings::snapshot::SETTINGS_BLOCK;
    use crate::settings::{Command, Settings};
    use crate::status::snapshot::STATUS_BLOCK;
    use crate::status::Status;

    // The snapshots: on, heating, P01 33, P04 1, inlet 27.5 C, no faults.
    fn settings() -> Settings {
        Settings::from_block(SETTINGS_BLOCK)
    }
    fn status() -> Status {
        Status::from_block(STATUS_BLOCK)
    }
    fn with_setting(s: Settings, addr: u16, value: u16) -> Settings {
        let mut raw = *s.raw();
        raw[usize::from(addr - SETTINGS_START)] = value;
        Settings::from_block(raw)
    }
    fn with_status(s: Status, addr: u16, value: u16) -> Status {
        let mut raw = *s.raw();
        raw[usize::from(addr - STATUS_START)] = value;
        Status::from_block(raw)
    }
    /// Apply what the manager asked for, as the heat pump would.
    fn run(s: Settings, command: Option<Command>) -> Settings {
        command.map_or(s, |c| s.apply(c).unwrap())
    }

    #[test]
    fn request_enables_in_one_write_then_holds() {
        let mut m = Manual::new();
        m.request(Some(45), 0, LEASE_MQTT_MS).unwrap();
        let c = m.step(1_000, &settings(), Some(&status()));
        assert_eq!(c, Some(Command::SetManualFreq(Some(45))));
        let s = run(settings(), c);
        assert!(s.manual_freq());
        // Same value again: nothing to write.
        m.request(Some(45), 20_000, LEASE_MQTT_MS).unwrap();
        assert_eq!(m.step(20_000, &s, Some(&status())), None);
        assert_eq!(m.reason(), Reason::None);
    }

    #[test]
    fn rate_limit_and_deadband_apply_to_changes_only() {
        let mut m = Manual::new();
        m.request(Some(45), 0, LEASE_MQTT_MS).unwrap();
        let s = run(settings(), m.step(0, &settings(), Some(&status())));

        // 1 Hz: below the step, never written.
        m.request(Some(46), 70_000, LEASE_MQTT_MS).unwrap();
        assert_eq!(m.step(70_000, &s, Some(&status())), None);

        // 5 Hz but too soon after the enable.
        m.request(Some(40), 30_000, LEASE_MQTT_MS).unwrap();
        assert_eq!(m.step(30_000, &s, Some(&status())), None);
        // Allowed once the interval has passed.
        let c = m.step(MIN_WRITE_INTERVAL_MS, &s, Some(&status()));
        assert_eq!(c, Some(Command::SetManualFreq(Some(40))));

        // Off is never held back, even right after a write.
        let s = run(s, c);
        m.request(None, MIN_WRITE_INTERVAL_MS + SETTLE_MS, 0)
            .unwrap();
        assert_eq!(
            m.step(MIN_WRITE_INTERVAL_MS + SETTLE_MS, &s, Some(&status())),
            Some(Command::SetManualFreq(None))
        );
    }

    #[test]
    fn lease_expiry_clears() {
        let mut m = Manual::new();
        m.request(Some(50), 0, LEASE_MQTT_MS).unwrap();
        let s = run(settings(), m.step(0, &settings(), Some(&status())));
        assert_eq!(m.lease_s(60_000), 120);
        let c = m.step(LEASE_MQTT_MS, &s, Some(&status()));
        assert_eq!(c, Some(Command::SetManualFreq(None)));
        assert_eq!(m.reason(), Reason::Lease);
        assert_eq!(m.requested_hz(), None);
    }

    #[test]
    fn found_on_at_boot_is_cleared() {
        let leftover = settings().apply(Command::SetManualFreq(Some(30))).unwrap();
        let mut m = Manual::new();
        assert_eq!(
            m.step(0, &leftover, Some(&status())),
            Some(Command::SetManualFreq(None))
        );
        assert_eq!(m.reason(), Reason::Boot);
    }

    #[test]
    fn power_mode_and_fault_refuse() {
        for (s, st, reason) in [
            (
                with_setting(settings(), 0x003f, 0x1070),
                status(),
                Reason::Power,
            ),
            (with_setting(settings(), 0x0040, 2), status(), Reason::Mode),
            (settings(), with_status(status(), 0x0008, 1), Reason::Fault),
            (settings(), with_status(status(), 0x001c, 3), Reason::Fault),
        ] {
            let mut m = Manual::new();
            m.request(Some(50), 0, LEASE_MQTT_MS).unwrap();
            assert_eq!(m.step(0, &s, Some(&st)), None, "{reason:?}");
            assert_eq!(m.reason(), reason);
        }
    }

    #[test]
    fn a_fault_while_running_clears() {
        let mut m = Manual::new();
        m.request(Some(50), 0, LEASE_MQTT_MS).unwrap();
        let s = run(settings(), m.step(0, &settings(), Some(&status())));
        let faulty = with_status(status(), 0x000c, 1 << 5);
        assert_eq!(
            m.step(SETTLE_MS, &s, Some(&faulty)),
            Some(Command::SetManualFreq(None))
        );
        assert_eq!(m.reason(), Reason::Fault);
    }

    #[test]
    fn setpoint_cut_out_latches_until_p04_below() {
        // P01 33, P04 2. Inlet is 0x000f in tenths.
        let s0 = with_setting(settings(), 0x004d, 2);
        let mut m = Manual::new();
        m.request(Some(50), 0, 10 * LEASE_MQTT_MS).unwrap();
        let s = run(s0, m.step(0, &s0, Some(&status())));
        assert!(s.manual_freq());

        // Reached 33.0: cut out.
        let warm = with_status(status(), 0x000f, 330);
        let c = m.step(SETTLE_MS, &s, Some(&warm));
        assert_eq!(c, Some(Command::SetManualFreq(None)));
        assert_eq!(m.reason(), Reason::Setpoint);
        let s = run(s, c);

        // 32.0 is only 1 C below: still latched, HA's resend is refused.
        let t = 2 * SETTLE_MS;
        m.request(Some(50), t, LEASE_MQTT_MS).unwrap();
        assert_eq!(
            m.step(t, &s, Some(&with_status(status(), 0x000f, 320))),
            None
        );
        assert_eq!(m.reason(), Reason::Setpoint);

        // 31.0 is P04 below: released, manual comes back.
        let t = 3 * SETTLE_MS;
        assert_eq!(
            m.step(t, &s, Some(&with_status(status(), 0x000f, 310))),
            Some(Command::SetManualFreq(Some(50)))
        );
        assert_eq!(m.reason(), Reason::None);
    }

    #[test]
    fn settle_time_prevents_requeueing() {
        let mut m = Manual::new();
        m.request(Some(50), 0, LEASE_MQTT_MS).unwrap();
        assert!(m.step(0, &settings(), Some(&status())).is_some());
        // The readback has not shown it yet; do not queue it twice.
        assert_eq!(m.step(1_000, &settings(), Some(&status())), None);
        assert!(m.step(SETTLE_MS, &settings(), Some(&status())).is_some());
    }

    #[test]
    fn out_of_range_requests_are_refused() {
        let mut m = Manual::new();
        assert!(m.request(Some(29), 0, LEASE_MQTT_MS).is_err());
        assert!(m.request(Some(91), 0, LEASE_MQTT_MS).is_err());
        assert_eq!(m.requested_hz(), None);
    }
}
