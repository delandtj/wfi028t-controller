# Manual compressor frequency: an MQTT number with a lease

**Status**: Accepted
**Date**: 2026-10-12
**Updated**: 2026-10-12 (implemented; deviations folded in: one
`SetManualFreq(Option<u8>)` command instead of `apply_all`, the lease
sensor rides with the counters, console syntax `set mfreq <hz>|off`)

---

## Context

The heat pump's mainboard picks the compressor frequency itself: it steps
through the F1-F10 ladder (0x005d-0x0066, 40..90 Hz) towards the setpoint,
and the only outside handles today are power, ECO/boost, mode and the
setpoints (`docs/register-map.md` "Controls").

The vendor protocol also lists a manual frequency: 0x003f bit 1 "manual
frequency" and 0x0043 "manual frequency setting". Tested on the real unit on
2026-10-12 with the console commands `set mfreq` / `set mfreq_hz` (commit
00a8c79):

| Time | Manual | Target / actual | Inlet | Outlet |
|---|---|---|---|---|
| before | off | 55 / 55 Hz | 25.0 | 27.0 |
| 16:04 | on, 50 Hz | 50 / 49-50 Hz | 25.0 | 27.0 |
| 16:07 | on, 30 Hz | 30 / 29-30 Hz | 25.0 | 26.0 |
| 16:20 | off | 50 / 49 Hz | 25.1 | 26.5 |

- The mainboard follows within ~5 s of the write and ramps at about
  1-2 Hz/s; the outlet settles within a minute.
- Clearing bit 1 hands control back; the target returned to the mainboard's
  own choice. 0x0043 keeps its last value and is ignored while bit 1 is
  clear.
- No faults, no rejected writes. ECO/boost (bit 6) was toggled by HA during
  the run and did not visibly change the manual target.

Home Assistant wants to use this (`~/Bosstraat/HA`, ADR 0002 there,
"Managed" heating mode): hold grid import under a limit by moving the
frequency. HA owns the decision; this ADR is the firmware side - what the
controller exposes, and what it guarantees when HA is wrong or gone.

Three properties of the hardware shape it:

1. **Manual mode is sticky.** The settings block survives a power cut
   (register map, "Controller behaviour"), so bit 1 does too. Nothing on the
   heat pump ends manual mode on its own.
2. **Manual mode probably ignores the setpoint.** Untested; the safe
   assumption is that the mainboard holds the manual frequency regardless of
   the water temperature.
3. **Every change is a whole-block write, three times**, and the block is
   most likely kept in EEPROM. A control loop that writes every few seconds
   could wear it out over a season.

---

## Decision

The controller exposes the manual frequency as **one MQTT number with a
lease**, and owns every safety rule around it. HA only ever asks for a
frequency; the firmware decides whether and when that reaches the bus.

- `number.wfi028t_manual_hz`, 30-90 Hz, plus a payload `off`.
- Every accepted value starts or refreshes a **lease** (default 180 s). When
  the lease runs out, the firmware clears bit 1. HA is expected to resend
  about every 30-60 s even when the value has not changed; a resend that
  changes nothing writes nothing.
- Writes to 0x0043 are **rate-limited and deadbanded** in the firmware.
  Enabling and disabling manual mode are never delayed.
- The firmware clears manual mode on its own at boot, on power-off, on a
  mode other than heat, on a fault, and when the water reaches the setpoint
  (with a latch until it has dropped by P04).
- The console commands from the test (`set mfreq`, `set mfreq_hz`) go
  through the same component, so they get the same rules.

---

## Architecture Overview

### Component Breakdown

1. **Model** ([`hp-model/src/settings.rs`](../../hp-model/src/settings.rs),
   [`hp-model/src/manual.rs`](../../hp-model/src/manual.rs))
   - `BIT_MANUAL_FREQ`, `REG_MANUAL_FREQ`, `Settings::manual_freq()` /
     `manual_freq_hz()`, and one command, `Command::SetManualFreq(Option<u8>)`:
     `Some(hz)` writes 0x0043 and sets bit 1 in the same block (30-90 Hz),
     `None` clears bit 1 and leaves 0x0043 alone.
   - `Command::SetPower(false)` clears bit 1 in the same write. No
     multi-command `apply` was needed.
   - `Status::any_fault()`: 0x0007-0x000d or 0x001c/0x001d nonzero.
   - `manual::Manual`: the rules below as a pure state machine with a
     caller-supplied clock, tested on the host.

2. **Manual-frequency manager** ([`fw/src/manual.rs`](../../fw/src/manual.rs), around `hp_model::manual::Manual`)
   - Owns the requested state: `Off` or `On { hz, lease_until }`, and the
     cut-out latch.
   - Inputs: requests (MQTT, console), each status/settings poll (to see
     inlet water, setpoint, P04, mode, power, fault bits, and what the heat
     pump actually holds), and a 1 s tick for the lease.
   - Output: commands into the existing [`master::COMMANDS`](../../fw/src/master.rs)
     queue. It never writes the bus itself, so the three-write sequence,
     readback check and outcome reporting stay as they are.
   - Rules, in priority order:

     | Rule | Trigger | Action |
     |---|---|---|
     | Boot | first settings read in master mode shows bit 1 set | clear bit 1 |
     | Lease | `lease_until` passed | clear bit 1, state `off`, reason `lease` |
     | Power | power off requested or read back | clear bit 1 in the same write as the power-off |
     | Mode | mode is not heat | refuse `on`, clear if set, reason `mode` |
     | Fault | any of 0x0007-0x000d nonzero (includes the water flow fault, 0x0008 bit 0), or 0x001c/0x001d nonzero | clear bit 1, reason `fault` |
     | Cut-out | inlet water >= active setpoint | clear bit 1, latch, reason `setpoint` |
     | Latch | latched and inlet > setpoint - P04 | refuse `on`, reason `setpoint` |
     | Rate | hz change < 2 Hz, or < 60 s since the last 0x0043 write | hold; write the latest value when allowed |

     Enabling writes 0x0043 and bit 1 together in one block write, so the
     mainboard never runs in manual mode with a stale 0x0043. Disabling is
     immediate and does not wait for the rate limit. After queueing any
     command the manager waits 8 s (`SETTLE_MS`) for the readback before it
     queues another, so a slow write is never queued twice.
   - The rate limit applies to 0x0043 only. Worst case with a busy HA loop:
     one block write per minute while running, i.e. under 1000 per day of
     full-time running. The values (2 Hz, 60 s, 180 s lease) are constants
     first; settings only if they turn out to need tuning in the field.

3. **Master** ([`fw/src/master.rs`](../../fw/src/master.rs))
   - Power-off from any source (MQTT, console, HA) is applied together with
     clearing bit 1, so the unit never comes back on in manual mode.
   - `write_command` spells the command for the notes and `last_command`
     (`mfreq 42`, `mfreq off`); the reason is in its own note
     (`manual frequency: reason - -> lease`).

4. **MQTT** ([`fw/src/mqtt/entity.rs`](../../fw/src/mqtt/entity.rs))
   - Command topic `wfi028t/manual_hz/set`, payload `30`..`90` or `off`.
     Out-of-range numbers are rejected like the setpoints are.
   - Discovery: `number.wfi028t_manual_hz` (30-90, step 1, unit Hz, mode
     box; `Kind::Number` gained a unit), `binary_sensor.wfi028t_manual_freq`
     (bit 1 as read back), `sensor.wfi028t_manual_reason` (`-`, `lease`,
     `power`, `mode`, `fault`, `setpoint`, `boot`: why the firmware last
     cleared or refused it), `sensor.wfi028t_manual_lease_s` (diagnostic,
     seconds left, 0 when off).
   - State document gains `manual_hz` (the held value while bit 1 is set,
     `null` otherwise), `manual_freq`, `manual_reason`, and
     `manual_lease_s`. The lease counts down every second, so it is written
     after the counters, where changes do not trigger a publish.
   - An accepted request is not logged or echoed in `last_command` (it
     arrives every 30-60 s); a refused one is, like any refused command.
   - Not in the HA card by default; the HA side decides where it shows.

5. **Console** ([`fw/src/cmd.rs`](../../fw/src/cmd.rs))
   - `set mfreq <hz>|off` is a request to the manager (the test's
     `set mfreq on|off` / `set mfreq_hz` are gone). A console request gets a
     10 min lease (it is a person at a terminal, not a loop).
   - `mode listen` first clears bit 1 (and waits for the outcome) if the
     heat pump holds it.
   - `status` keeps `hp_mfreq`, `hp_mfreq_hz`, `hp_target_hz` and gains
     `manual_reason=`, `manual_lease_s=`.

6. **Register map** ([`docs/register-map.md`](../register-map.md))
   - 0x003f bit 1 and 0x0043 move from "vendor only" to the Controls table,
     with the 2026-10-12 test as evidence.

### Data Flow / Interaction

```
HA automation (every 30-60 s)
   | wfi028t/manual_hz/set  "42" | "off"
   v
mqtt task --> manual manager <-- console "set mfreq_hz 42"
                 |  ^
   lease, rules, |  | status + settings snapshot (1 s)
   rate limit    v  |
            master::COMMANDS --> bus: whole block x3 --> heat pump
                                           |
                     readback (next poll) -+--> manual_freq, manual_hz,
                                                manual_reason, lease
                                                --> MQTT state document
```

---

## Alternatives Considered

### Expose bit 1 and 0x0043 as two plain entities (switch + number)
- **The idea**: the same shape as boost and P01: HA writes, firmware relays.
- **Optimizes for**: the least firmware code; HA has full control.
- **Sharpest tradeoff**: an HA restart, a broken automation or a lost
  broker leaves the unit in manual mode at a fixed frequency, possibly
  ignoring the setpoint, until someone notices.
- **Bets on**: HA never failing while manual mode is on. Not a bet worth
  making on a 400 V machine heating a pool unattended.

### The control loop in the firmware
- **The idea**: the controller reads the P1 meter itself (HomeWizard local
  API or an MQTT topic) and runs the import-limit loop on the ESP32.
- **Optimizes for**: working without HA; tighter loop timing.
- **Sharpest tradeoff**: duplicates what HA already has - the smoothed grid
  power, min run/off, the capacity-tariff allowance and its guard - and puts
  household energy policy into heat pump firmware.
- **Bets on**: HA being less reliable than the controller. If that turns
  out true, the manager here is the right foundation for it: the loop would
  become one more request source with the same lease.

### Reshape the F1-F10 ladder instead of manual mode
- **The idea**: write a lower or flatter ladder to cap the frequency and
  let the mainboard keep the control.
- **Optimizes for**: the mainboard keeps its own thermostat and protections.
- **Sharpest tradeoff**: rewrites ten factory parameters to move one value,
  and it only caps; it cannot hold a chosen frequency.
- **Bets on**: a cap being enough. It is a fine "quiet mode" and could
  come later, but it does not do import-following.

### Lease refreshed by MQTT retained messages or broker liveness
- **The idea**: no explicit resend; treat the MQTT connection itself as
  the lease.
- **Optimizes for**: fewer messages.
- **Sharpest tradeoff**: the broker stays up while HA's automation is dead
  (HA restart, automation disabled, template error). The lease has to prove
  the decision-maker is alive, not the transport.
- **Bets on**: HA and the broker failing together. They do not.

---

## Consequences

### Positive
- HA can steer the frequency; the "Managed" mode becomes possible.
- Every failure of HA, the broker or the network ends in the mainboard's
  own control within the lease time, which is today's behaviour.
- The EEPROM write rate is bounded by the firmware, whatever HA sends.
- A power-off can no longer leave manual mode armed for the next power-on.

### Negative
- One more stateful component in the firmware, with rules that need
  testing against a mock heat pump (`tools/bench/hp_mock.py`).
- The setpoint cut-out duplicates what the mainboard does in normal mode;
  if the mainboard turns out to honour the setpoint in manual mode too, the
  cut-out is redundant (harmless).
- HA must keep resending; a quiet loop is indistinguishable from a dead one.

### Risks
- **Manual mode bypasses a protection we do not know about** (discharge
  temperature, current limit, defrost, anti-freeze). Mitigation: the fault
  rule, the 30 Hz floor, and a first season with the manual mode watched;
  check the TP0-TP4 behaviour by logging discharge temperature at 90 Hz.
- **Long running at 30 Hz** may be poor for oil return in some compressors.
  Mitigation: HA ADR 0002 limits time at the floor; the vendor can be asked.
- **The stock controller is plugged back in** while bit 1 is set: its
  boot write sends the block it last knew, which may or may not clear bit 1.
  Mitigation: the firmware clears bit 1 on its way out of master mode
  (`mode listen`) as well.

---

## What an Expert Would Ask

**Q: What does the mainboard do with defrost while in manual mode?**
A: Unknown. In normal mode it ramps the compressor down, reverses and
defrosts on its own. If manual mode suppresses that, the coil ices up. Not
handled beyond the fault rule; the first cold-weather run is the test. If
the defrost flag (0x0003 bit 7, or the four-way valve, 0x0004 bit 6;
never set in the capture so far) is seen during a manual run, that answers
it; if ice is seen without it, add a rule that
clears manual mode below an ambient threshold (the HA side can also simply
not use Managed below ~7 C ambient).

**Q: Why 180 s for the lease, and what happens in that window?**
A: HA resends every 30-60 s, so 180 s survives two lost messages. In the
worst case the unit runs 3 min at the last requested frequency after HA
died - the same thing it does every time the user changes a setting. A
shorter lease means more false expiries on a slow broker; a longer one is
more time at a possibly wrong frequency. It is a constant, easy to change.

**Q: The cut-out uses inlet water. Is that the right sensor?**
A: The manual's thermostat and P01 are on inlet water (A01), and that is
what the mainboard compares in normal mode as far as the capture shows
(heating demand drops when inlet reaches P01). The firmware uses the same,
so the behaviour does not change between manual and automatic.

**Q: How do you know a write actually took effect, given rate limiting?**
A: The readback (`manual_freq`, `manual_hz` from the next settings poll)
is what is published, never the request. HA sees the real state and the
target frequency (0x001a) moving. The rate limiter only delays; the latest
request always wins.

**Q: What about EEPROM wear from the lease resends?**
A: None: a resend that does not change the held value is a firmware-only
event. Only a change of at least 2 Hz, at most once a minute, or an on/off
transition, causes a block write.

---

## Implementation Plan

### Decisions you will probably want to tweak

- **One number with `off`** vs. a switch plus a number.
  - Alternative: `switch.wfi028t_manual_freq` + `number.wfi028t_manual_hz`.
  - Cost to change later: small (entity ids are new), but HA automations
    would need editing. One entity means one message carries both "on" and
    the value, so the two can never disagree.
- **Lease 180 s, rate limit 60 s / 2 Hz** as constants.
  - Alternative: settings over the console/MQTT.
  - Cost to change later: a firmware update; promote to settings only if
    field use asks for it.
- **Cut-out on inlet >= setpoint with a P04 latch.**
  - Alternative: no cut-out, trust HA to stop at the setpoint.
  - Cost to change later: trivial; but removing a safety rule should need a
    reason.

### Known unknowns and how the plan absorbs them

- **Does manual mode ignore the setpoint?** Default: assume yes (cut-out).
  Signal: a manual run past P01 where the mainboard stops by itself.
- **Defrost under manual mode.** Default: rely on the mainboard plus the
  fault rule. Signal: ice, or no defrost flag during cold manual runs.
- **ECO bit in manual mode.** Default: ignored by the mainboard (consistent
  with the test). Signal: target frequency moving on an ECO toggle while
  manual.

### The mechanical work

- The model command and power-off coupling, the manager module, MQTT command parsing and discovery, console routing, status
  fields, register-map update.
- Tests: model tests for the command and power-off coupling; manager rule
  tests on the host (pure state machine, fed snapshots and a clock); the
  MQTT entity/json host tests; a live check on the unit.

### Review asks

1. One `number` with an `off` payload, or a switch plus a number?
2. Lease 180 s, rate limit 60 s / 2 Hz: OK as starting constants?
3. Keep the setpoint cut-out in the firmware even though HA also stops at
   the setpoint (yes / no)?
4. Clear manual mode on `mode listen`, so the stock controller never
   inherits it (yes / no)?

---

## Open Questions

**Architecture-changers**
- [ ] Does the mainboard honour P01 in manual mode? (Decides whether the
  cut-out is a safety net or the only thermostat.)

**Behavior definers**
- [ ] Defrost behaviour in manual mode (first cold run).
- [ ] Which vendor fault flags should end manual mode: all of 0x0007-0x000d,
  or only the ones seen on this unit?
- [ ] Minimum frequency: the vendor ladder floor is 30 Hz; does the vendor
  advise a higher sustained minimum for oil return?
