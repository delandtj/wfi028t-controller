# Rust replacement controller for the WFI-028T/035T heat pump

**Status**: Accepted
**Date**: 2026-10-03
**Updated**: 2026-10-03 (review asks settled: MQTT, core as path dependency, wall display dropped)

---

## Context

The WFI-028T/035T full-inverter pool heat pump is driven by a wired controller
that is the Modbus RTU master on an RS-485 bus (9600 8N1, heat pump PCB =
slave 0x01). Its Tuya WiFi integration has broken repeatedly after vendor app
updates. The goal is to replace the wired controller with our own device that
Home Assistant can drive locally, with no cloud.

What we know, from a sniffed capture and a controller readout (see
[`docs/register-map.md`](../register-map.md)):

- The controller polls two blocks once per second: status 0x0000-0x003e
  (63 registers) and settings 0x003f-0x0081 (67 registers). The heat pump
  answers in about 150 ms.
- Every setting change is written as the WHOLE settings block (function 0x10,
  67 registers), three times, 500 ms apart, each acked in about 24 ms.
- Confirmed registers: on/off, P05 and ECO/boost bits in 0x003f; mode in
  0x0040; setpoints P01-P04 in 0x0041/0x0042/0x004a/0x004d (whole degrees);
  all fourteen A01-A14 sensors in 0x000f-0x0025 with mixed scaling (/10, /2,
  x1); status bits for unit on, water pump, boost.
- The WFI Modbus sheet says the stock controller must be unplugged when an
  external master is used.

Existing assets live in the sibling sniffer project
(`../stm32-modbus-sniffer`, git): its ESP32-C6 firmware (`fw-esp32c6`)
already has esp-hal + embassy, WiFi with reconnect, UART1 RX with t3.5
framing, settings persisted in flash, a status LED, and a TCP line stream
consumed by its capture daemon (`tools/`, `sniffer-capture` /
`sniffer-analyze`, installed as a systemd service on the capture server).
Its `core` crate (`modbus-sniffer-core`) holds CRC, framing rules, the line
ring and formatting, and is host-testable. The RS-485 module switches
direction automatically, so TX needs only one extra GPIO.

Constraints: one master on the bus; the heat pump keeps running on its own
PCB logic between commands; the device lives in a poolhouse (remote updates
matter); the user works in Rust and prefers it over ESPHome.

---

## Decision

Build the replacement controller in Rust on the existing ESP32-C6 hardware,
in this project (`wfi028t-controller`), with the **sniffer's functions
integrated**: the controller firmware keeps the sniffer's WiFi, LED, flash
settings and TCP capture stream, and reuses `modbus-sniffer-core` (as a git or
path dependency, or vendored if it needs controller-specific changes). The
sniffer project stays as the separate receive-only tool. A `listen-only`
mode (cargo feature or persisted setting) in this firmware never transmits,
for safe bring-up and as a fallback.

The controller:

1. **Imitates the stock controller on the bus**: the same 1 s poll of both
   blocks, the same response timeouts, and the same whole-block,
   read-modify-write, three-times write for every settings change. No
   single-register writes unless the bench test (below) proves they work and
   we choose to switch.
2. **Owns a typed register model** in a host-testable `no_std` crate: decode
   and encode with per-register scaling and bit fields, range validation
   from the manual (P01 8-40, P02 8-28, P03 8-40, P04 1-18, mode 1/2/7).
3. **Talks to Home Assistant over MQTT with HA discovery**: one device, the
   entity list from the register map's "Target" section, an availability
   topic, and command topics that go through validation before anything
   reaches the bus.
4. **Keeps the capture stream**: every frame it sends or receives goes out on
   TCP 4000 like the sniffer does today (own frames labelled `TX`), so the
   capture daemon and analyzer keep working and every command is auditable.
5. **Fails safe**: polling continues regardless of WiFi/MQTT state; the last
   good settings stay in the heat pump (it runs on its own logic); commands
   are rate-limited; nothing is written unless the settings block was read
   within the last 2 s.

A bench test with the FTDI adapter and a Python emulator
([`tools/bench/ctl_bench.py`](../../tools/bench/ctl_bench.py)) runs BEFORE
implementation, to answer the questions that shape the design: does the heat
pump accept single-register writes, and how long does it tolerate a silent
master before raising E09.

---

## Architecture Overview

### Component Breakdown

1. **Register model** (`hp-model/`, a `no_std` crate in this repo)
   - Typed `Status` and `Settings` structs decoded from the raw 63/67-register
     blocks; `Settings::encode()` returns the raw block with only the changed
     fields touched (unknown registers pass through untouched).
   - Scaling table per field (raw/10, raw/2, x1), bit accessors for 0x003f,
     0x0004, 0x0005.
   - Validation: `Settings::apply(Command) -> Result<Settings, Rejected>`.
   - No dependencies (CRC-16 is local: `modbus-sniffer-core` would pull
     embassy-sync into a pure data model); fully unit-tested on the host
     against frames and blocks captured from the real bus.

2. **Bus master** (`fw/src/master.rs`)
   - Owns UART1 TX+RX (RX only in `listen-only` mode). A single task runs the cycle: read status, wait,
     read settings, wait, and inserts pending writes after a settings read.
   - Request/response with a response timeout (about 500 ms) and t3.5 framing
     reused from the sniffer receiver.
   - Write procedure: take the last settings block (fresh < 2 s), apply the
     command via the model, send 0x10 three times 500 ms apart, verify the
     ack each time and the readback on the next poll.
   - Link health: consecutive timeouts -> `link=down`, exposed to MQTT and the
     LED.
   - Interfaces: `Signal<Command>` in, `Watch<Snapshot>` out (latest
     decoded status + settings + link state).

3. **MQTT + HA discovery** (`fw/src/mqtt.rs`)
   - MQTT 3.1.1 client over embassy-net TCP, hand-rolled (`fw/src/mqtt/`):
     rust-mqtt 0.6 is MQTT 5 only, its last 3.1.1 release is on an older
     embedded-io-async, and minimq is MQTT 5 + serde. QoS 0 publish/subscribe,
     retain, LWT and ping are all that is needed.
   - On connect: publish retained discovery configs for every entity, the
     availability topic, then state.
   - State publishing: on change, plus a full refresh every 60 s.
   - Command topics map to `Command` values; invalid payloads are rejected and
     logged, never forwarded.

4. **Capture stream** (ported from the sniffer: line ring + `net.rs`)
   - Same TCP 4000 protocol, so the existing capture server keeps working;
     frame lines gain a `TX` direction label for frames we sent. The
     sniffer's host parser (`tools/src/device.rs` there) must accept `TX`
     lines.
   - Log compaction (see the sniffer HANDOFF): a full day is ~346k lines /
     108 MiB because the same two polls repeat every second. The capture side
     should log only changes plus periodic snapshots and liveness counters.

5. **OTA updates** (`fw/src/ota.rs`)
   - Two-slot OTA partition layout, image pushed over HTTP or pulled from a
     URL; rollback if the new image does not confirm itself within N minutes
     of boot. Crate choice open.

6. **Hardware delta**
   - RS-485 module TX-side pin -> one free GPIO (proposal: GPIO5) as UART1 TX.
   - Stock controller unplugged; kept as a fallback behind a plug or switch.
   - Power: USB charger, or the controller cable's 12 V through a buck
     converter.

### Data Flow / Interaction

```
 Home Assistant <--MQTT--> mqtt task --Command--> bus master --Modbus RTU--> heat pump PCB
                     ^                                |   ^                     (slave 0x01)
                     |                                |   |
                     +------ Snapshot (Watch) <-------+   +-- 1 s poll, whole-block writes
                                                      |
                                                      v
                                        line ring -> TCP 4000 -> sniffer-capture (server)
```

---

## Alternatives Considered

### ESPHome with `modbus_controller`
- **The idea**: YAML config on the same ESP32-C6, native HA API.
- **Optimizes for**: time to a first working version and zero HA plumbing.
- **Sharpest tradeoff**: whole-block, triple, read-modify-write writes and
  safety logic live in untestable lambdas fighting the framework; the
  sniffer/capture functions would need a second device.
- **Bets on**: the heat pump accepting ESPHome's single-register writes and
  tolerating its poll timing. If the bench test shows single writes work
  fine, this alternative gets cheaper, though the testability point stands.

### Keep the stock controller, add a second master in the polling gaps
- **The idea**: leave the wired controller in place (display, buttons) and
  inject our reads/writes in the ~300 ms gaps of its 1 s cycle; the generic
  factory protocol document describes exactly this "PC virtual master"
  arrangement.
- **Optimizes for**: keeping the physical controller and its display working.
- **Sharpest tradeoff**: bus collisions if timing drifts, and the stock
  controller rewrites the whole settings block on every user action, so the
  two masters can overwrite each other's settings.
- **Bets on**: the stock controller's timing being stable enough to schedule
  around, and the user rarely touching the wall controller. Worth revisiting
  if losing the wall display turns out to be a problem.

### Rust firmware with the ESPHome native API instead of MQTT
- **The idea**: speak ESPHome's protobuf API from Rust so HA sees an ESPHome
  device.
- **Optimizes for**: HA integration without an MQTT broker.
- **Sharpest tradeoff**: no maintained Rust implementation; reverse-tracking
  ESPHome's API versions becomes our job.
- **Bets on**: the user not running an MQTT broker. Revive if adding a broker
  is unwelcome.

---

## Consequences

### Positive
- Local control, no cloud, no vendor app.
- Bus behaviour matches the stock controller, so the heat pump sees nothing
  unusual.
- Register model and command validation are unit-tested on the host.
- Every command is recorded in the existing capture log.
- One hardware platform for sniffing and controlling.

### Negative
- MQTT discovery and OTA are new code to own.
- The wall display goes away (HA becomes the UI).
- Clock and timers move to HA automations.

### Risks
- **Unknown write semantics** (partial writes, unknown registers in the block
  being meaningful). Mitigation: always read-modify-write the whole block,
  bench test first.
- **E09 when the master stops** (crash, reboot, OTA). Mitigation: measure the
  tolerance on the bench; keep reboot time well under it; watchdog.
- **Mis-scaled or out-of-range writes**. Mitigation: typed model with range
  checks, unit tests from real frames, allowlist of writable fields.
- **Transmitting before the stock controller is unplugged** (two masters).
  Mitigation: listen before the first transmit and refuse if another master
  is active (like the bench script does); `listen-only` mode for bring-up.

---

## What an Expert Would Ask

**Q: The settings block has 67 registers and we understand about 8 of them.
Isn't writing the whole block back dangerous?**
A: It is what the stock controller does on every change, and we write back
exactly the values we just read, changing only validated known fields. The
real danger would be writing a stale or invented block, which is why a write
requires a read less than 2 s old. Not handled: values the heat pump changes
on its own between our read and our write within those 2 s; the stock
controller has the same race and it has not been a problem.

**Q: What happens when the ESP32 crashes or reboots mid-season?**
A: The heat pump keeps its last settings and runs on its own logic; the risk
is E09 and whatever the PCB does on E09 (probably stops). The bench test
measures how long it tolerates silence. Reboot plus WiFi is not on the
critical path (polling starts before WiFi), so the bus gap after a reset is
about a second. Not yet known: whether E09 latches and needs a power cycle.

**Q: Why trust the triple write instead of verifying once?**
A: We do both: three writes like the stock controller, each ack checked, then
readback on the next poll. If the readback disagrees, the command is reported
failed to HA and logged; no automatic retry loop beyond that.

**Q: Who resolves the conflict if HA and the (unplugged) stock controller
disagree?**
A: There is no stock controller on the bus in this design. If it is plugged
back in as a fallback, our device must not be: the plug or switch is the
interlock. Not enforced in software.

**Q: Isn't 9600 baud with auto-direction RS-485 timing-sensitive for a
master?**
A: The module switches direction from the TX line itself, so turnaround is
handled in hardware; the heat pump waits ~150 ms before answering, far
longer than any turnaround. The sniffer's t3.5 framing is already proven on
this bus.

---

## Implementation Plan

### Decisions you will probably want to tweak
- **Choice**: MQTT with HA discovery. **Alternative**: ESPHome native API.
  **Cost to change later**: moderate; the `mqtt` task is isolated behind
  `Command`/`Snapshot`.
- **Choice**: separate project that integrates the sniffer functions;
  sniffer repo stays receive-only. **Alternative**: a `controller` feature
  inside the sniffer firmware. **Cost to change later**: low; the shared
  pieces are `modbus-sniffer-core` and a few firmware modules.
- **Choice**: whole-block writes always. **Alternative**: single-register
  0x06 writes if the bench proves them. **Cost to change later**: low; one
  function in the bus master.
- **Choice**: `TX` label on our own frames in the capture stream.
  **Alternative**: a separate status line per command. **Cost to change
  later**: low, but it is a wire-format change the host tools must follow.

### Known unknowns and how the plan absorbs them
- **Single-register writes**: default is whole-block; switch only if the bench
  shows 0x06 or 0x10 x1 is acked AND read back correctly.
- **E09 tolerance**: default target is "never more than 2 s without a poll";
  if the bench shows a much shorter tolerance, polling moves to a
  higher-priority executor.
- **Status bits** (compressor, defrost, faults): default is "raw word sensors
  plus confirmed bits only"; add entities as the multi-day capture confirms
  them.
- **MQTT and OTA crates**: MQTT ended up hand-rolled (see component 3). OTA:
  default the esp-bootloader OTA support; pivot if it does not build against
  esp-hal 1.2. Flash is ~500 KB of .text already, so check the two-slot fit.

### The mechanical work
- Register model module with unit tests from captured frames.
- Bus master task, `listen-only` mode, UART TX on GPIO5.
- MQTT task with discovery for the register map's Target entities.
- Port from the sniffer firmware: WiFi/net, LED, flash settings, line ring.
- `TX` label in core formatting and the sniffer's host parser.
- Log compaction on the capture side.
- OTA with rollback.
- README/HANDOFF/register-map updates; wiring section for TX.

Review asks (settled 2026-10-03):
1. MQTT with HA discovery: yes, a broker on the LAN is fine.
2. `modbus-sniffer-core`: path dependency on `../stm32-modbus-sniffer/core`
   (that repo has no remote yet); vendor only if the controller needs changes
   in it.
3. Wall display loss: accepted; the "second master in the gaps" alternative
   is not pursued.

---

## Open Questions

**Architecture-changers**
- [ ] Does the heat pump accept single-register writes (0x06, 0x10 x1)?
      Bench test.
- [ ] How long until E09 when the master goes silent, and does E09 latch?
      Bench test.
- [x] MQTT broker available on the LAN? Yes: MQTT with HA discovery.

**Behavior definers**
- [ ] What should the device do on E09 or a fault: report only, or attempt a
      recovery (e.g. off/on)? Proposal: report only.
- [ ] Remaining status bits (compressor running, defrost, error words):
      from the multi-day capture.
- [ ] Mode values for cooling (2) and auto (7) on this unit: confirm by
      capture.
