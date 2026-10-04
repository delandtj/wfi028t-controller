# wfi028t-controller

A local, cloud-free replacement for the wired controller of a WFI-028T/035T
full-inverter pool heat pump (380-415V/3N), a Tuya rebrand sold as W'Eau whose
app integration kept breaking. The device becomes the Modbus RTU master on the
heat pump's RS-485 bus and exposes it to Home Assistant.

**Status: design.** Nothing is built yet. Start with the ADR and the bench test.

## Start here

1. [`docs/adr/0001-rust-replacement-controller.md`](docs/adr/0001-rust-replacement-controller.md)
   - the design (Proposed): Rust on an ESP32-C6, behaves exactly like the
   stock controller on the bus, MQTT with HA discovery, capture stream kept.
   Its review asks and open questions come first.
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

Same as the sniffer, plus TX:

- ESP32-C6-DevKitC-1 (2.4 GHz WiFi only), RS-485 module (isolated,
  automatic direction): VCC 3V3, GND, RXD -> GPIO4, and its TX-side pin ->
  a free GPIO (proposal GPIO5) as UART1 TX.
- Bus: controller cable green/yellow = A/B (red/black = 12 V). On this
  installation the module's A/B had to be swapped relative to the labels:
  inverted bits show up as `7f`/`fe`/`ff` garbage.
- The stock controller is unplugged when this device is the master; keep it
  as a fallback, never both on the bus.

## Next steps

1. Bench test with `ctl_bench.py` (stock controller unplugged, sniffer still
   logging): poll, whole-block write, single-register write, E09 tolerance.
2. Settle the ADR's review asks (MQTT broker, core reuse, wall display).
3. Map the remaining items: cooling/auto mode values, defrost and fault bits
   (from the sniffer's multi-day capture).
4. Log compaction in the capture pipeline (design in the sniffer HANDOFF):
   ~346k lines / 108 MiB per day today, nearly all repeated polls.
5. Then firmware, per the ADR.
