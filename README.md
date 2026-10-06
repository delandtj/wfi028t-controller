# wfi028t-controller

A local, cloud-free replacement for the wired controller of a WFI-028T/035T
full-inverter pool heat pump (380-415V/3N), a Tuya rebrand sold as W'Eau whose
app integration kept breaking. The device becomes the Modbus RTU master on the
heat pump's RS-485 bus and exposes it to Home Assistant.

**Status: in service since 2026-10-04.** The firmware runs the heat pump as
bus master; updates go over the network (see "Firmware updates" below). Start
with the ADRs.

## Start here

1. [`docs/adr/0001-rust-replacement-controller.md`](docs/adr/0001-rust-replacement-controller.md)
   - the design (Proposed): Rust on an ESP32-C6, behaves exactly like the
   stock controller on the bus, MQTT with HA discovery, capture stream kept.
   Its review asks and open questions come first.
   [`docs/adr/0002-ota-and-console-port.md`](docs/adr/0002-ota-and-console-port.md)
   adds signed updates over the network, a rollback bootloader and a second
   TCP client slot.
2. [`docs/register-map.md`](docs/register-map.md) - everything known about the
   bus: framing, the controller's poll/write behaviour, confirmed registers,
   the A01-A14 sensors (mixed scaling!), P01-P05, status bits, and the target
   entity list for Home Assistant with a mapping status per row.
3. [`tools/bench/ctl_bench.py`](tools/bench/ctl_bench.py) - Python bench test
   that plays the stock controller over an FTDI RS-485 adapter. Run it before
   writing firmware; it answers the questions that shape the design.

## What is known (2026-10-03)

- Modbus RTU, 9600 8N1, controller = master, heat pump PCB = slave 0x01.
- The controller polls 0x0000 x63 (status) and 0x003f x67 (settings) once
  per second; the heat pump answers in ~150 ms.
- Every settings change is the whole 67-register block, written three times
  500 ms apart (function 0x10).
- 0x003f is the main bit field: bit 0 on/off, bit 4 P05, bit 6 ECO (cleared =
  full power). Mode is 0x0040. Setpoints are whole degrees; sensors are mostly
  raw/2, inlet water raw/10, exhaust raw.
- The vendor's sheet says the stock controller must be unplugged when an
  external master is used. It is wrong about the boost bit, so trust captures
  over documents.

Vendor documents are in [`docs/vendor/`](docs/vendor) with plain-text
extractions. `communication-protocol-v1.3.2-eng` is for a different product
(air-to-water) and does NOT match this bus; it may still describe extra areas
of the same PCB (0x0300 user area, 0x0360 version), untested.

[`docs/home-assistant-dashboard.md`](docs/home-assistant-dashboard.md) is the
HA side: the entity ids the firmware publishes and a dashboard built from
them, including why there is no `climate` entity.

## Relation to the sniffer project

The bus was mapped with `../stm32-modbus-sniffer` (git, its own repo):
an ESP32-C6 + isolated auto-direction RS-485 module that captures the bus and
streams frames over WiFi (TCP 4000) to `sniffer-capture`, a systemd service on
the capture server that writes daily logs to /var/lib/modbus-sniffer;
`sniffer-analyze` turns logs into a protocol map.

**The sniffer's functions are to be integrated here**, not rewritten:

- Reuse `modbus-sniffer-core` (CRC, t3.5 framing, line ring, formatting,
  bus config, command parser) as a git/path dependency, or vendor it if the
  controller needs changes.
- Port from `fw-esp32c6`: WiFi/DHCP/reconnect (`net.rs`), status LED
  (`led.rs`), flash settings (`settings.rs`), the multi-consumer line ring and
  the TCP 4000 stream, so the existing capture server keeps logging
  everything, including the frames this controller sends (labelled `TX`).
- Keep a `listen-only` mode that never transmits: safe bring-up, and a
  fallback that is functionally the sniffer.

The sniffer repo stays as the receive-only tool.

## Hardware

Wiring, build and flash sheet: [docs/wiring.html](docs/wiring.html), also
online at <https://claude.ai/artifact/Swm2knzTawDmHsjwo6iN2h>.

Same as the sniffer, plus TX:

- ESP32-C6-DevKitC-1 (2.4 GHz WiFi only), RS-485 module (isolated,
  automatic direction): VCC 3V3, GND, RXD -> GPIO4, and its TX-side pin ->
  a free GPIO (proposal GPIO5) as UART1 TX.
- Bus: controller cable green -> module A, yellow -> module B (red/black =
  12 V, not connected). Module markings vary between makers: if the bus reads
  as `7f`/`fe`/`ff` garbage (inverted bits), swap green and yellow at the
  module.
- The stock controller is unplugged when this device is the master; keep it
  as a fallback, never both on the bus.

## Network ports

| Port | Who | What |
|---|---|---|
| 4000 | the capture daemon (`wfi-controller-capture.service`) | line stream with replay; its cursor is the capture, so nothing else should hold this |
| 4001 | you | console: same lines and the same commands, live tail, no replay |
| 4002 | `fw-ota` | signed firmware push, binary |

Neither 4000 nor 4001 is authenticated: the LAN is trusted for commands. 4002
accepts nothing that is not signed with the OTA key.

```sh
printf 'status\n' | nc wfi-controller 4001    # one command and the answer
nc wfi-controller 4001                        # watch the bus, type commands
```

## Firmware updates

### Once, over USB (this installs the layout)

```sh
cd fw
cargo run --release          # the runner does all of it (see .cargo/config.toml)
```

That writes the rollback bootloader
([`fw/bootloader/`](fw/bootloader)), the two-slot partition table
([`fw/partitions.csv`](fw/partitions.csv)), the app into `ota_0`, and erases
`otadata` so no stale slot selection survives. On the first boot the bootloader
writes a fresh `otadata` entry for `ota_0`, so `status` shows `ota=valid`
straight away. `nvs` is not touched, so the
bus, mode and MQTT settings survive - if they do not come back, set them again
(`mode master`, `mqtt host <ip>`, `bus 9600 8N1`).

Rebuild the bootloader only if its config changes:
`fw/bootloader/build.sh` (podman, `espressif/idf:release-v6.1`).

### After that, over the network

```sh
cd fw && cargo build --release
cargo run -p fw-ota -- push wfi-controller --wait
```

`push` builds the image with `espflash save-image`, signs a header for it,
sends it to port 4002 and prints what the device says. It exits 0 only on
`ok rebooting`. `--wait` then polls the console port until the new image
confirms itself.

What the device does with it (ADR 0002): checks the signature before erasing
anything, writes the standby slot, verifies the SHA-256, points `otadata` at
it and reboots. The new image runs **on probation** and marks itself good only
after the bus works (`ota=valid` in `status`). If it crashes, wedges or cannot
do the job within 120 s, the bootloader boots the previous image again
(`ota=aborted`). Nothing about this touches the heat pump's settings, and the
bus keeps being polled throughout; the reboot itself is a gap of under a
second, because a planned reboot skips the 3 s silence check.

### The signing key

- Private key: `~/.config/wfi028t/ota-signing.key`, mode 0600, **never in the
  repo**. Created once with `cargo run -p fw-ota -- keygen`. Back it up
  (a password manager, or any encrypted backup - it is 64 hex characters).
- Public key: [`fw/ota-signing.pub`](fw/ota-signing.pub), committed, compiled
  into the firmware with `include_bytes!`. That is why it is in git: it is
  public by definition, and the firmware has to carry it.
- **If the private key is lost**: no network update is possible any more; the
  running firmware accepts nothing else. Run `keygen` again (move the old
  `fw/ota-signing.pub` aside first, `keygen` refuses to overwrite), then
  USB-flash once so the device carries the new public key. Nothing else is
  lost - the heat pump and the settings are untouched.
- **If the private key leaks**: anyone on the LAN can push firmware. Same fix:
  new key pair, one USB flash.

## The `status` line

One line, `# status ...`, from both the console and the capture stream. The
first fields are the sniffer's (`modbus-sniffer-core`), the rest this
firmware's; new fields are only ever appended, because host-side parsers read
`# ` lines as opaque text.

| Field | Meaning |
|---|---|
| `uptime_ms` | since boot |
| `bus=<baud> <fmt>` | UART configuration in force |
| `wifi=`, `ip=`, `rssi=` | link state (`?` when unknown) |
| `frames=`, `bad_crc=`, `uart_errors=` | received frames and errors |
| `dropped=`, `usb_dropped=` | lines the TCP / USB consumer lost |
| `mode=` | `listen` or `master` |
| `link=` | is the heat pump answering |
| `requests=`, `status_ok=`, `settings_ok=`, `timeouts=`, `bad_responses=`, `echoes=`, `foreign=` | bus counters |
| `writes=`, `write_failures=`, `settings_age_ms=` | write path |
| `hp_power=`, `hp_boost=`, `hp_mode=`, `hp_setpoint=` | decoded settings |
| `hp_inlet_dc=`, `hp_outlet_dc=`, `hp_hz=`, `hp_fault=` | decoded status (tenths of a degree) |
| `status_age_ms=`, `snapshot_age_ms=` | how fresh the decoded blocks are |
| `mqtt=`, `mqtt_host=`, `mqtt_user=` | broker state and configuration (never the password) |
| `mqtt_published=`, `mqtt_received=`, `mqtt_dropped=`, `mqtt_failures=` | MQTT counters |
| `ota=` | `pending` (on probation), `valid` (confirmed; also right after a USB flash, because the bootloader fills the erased `otadata` in for `ota_0` on first boot), `aborted`/`invalid` (came back from a rollback), `undefined` (no verdict recorded for this slot), `unknown` (no OTA data) |
| `console=` | `connected` or `idle` on port 4001 |

## Next steps

1. Bench test with `ctl_bench.py` (stock controller unplugged, sniffer still
   logging): poll, whole-block write, single-register write, E09 tolerance.
2. Settle the ADR's review asks (MQTT broker, core reuse, wall display).
3. Map the remaining items: cooling/auto mode values, defrost and fault bits
   (from the sniffer's multi-day capture).
4. Log compaction in the capture pipeline (design in the sniffer HANDOFF):
   ~346k lines / 108 MiB per day today, nearly all repeated polls.
5. Then firmware, per the ADR.
