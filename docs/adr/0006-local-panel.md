# Local panel: a controller board with a screen and a knob

**Status**: Proposed
**Date**: 2026-10-07

---

## Context

The controller today is an ESP32-C6-DevKitC-1 plus an isolated RS-485 module,
wired by hand (README "Hardware", `docs/wiring.html`). Everything it knows is
reachable only over the network: Home Assistant over MQTT, the line ports
4000/4001, and - once ADRs 0003/0004 are built - a web page and a setup access
point. Standing at the heat pump, there is nothing to look at and nothing to
turn.

The stock wired controller it replaces is a panel: a display, a few keys, on a
4-wire cable from the heat pump (A, B, and 12 V on red/black, which we leave
unconnected today). Taking its place properly means taking its form as well:

- show the important running values without a phone (water in/out, setpoint,
  mode, compressor, faults);
- set the few things people change by hand (on/off, mode, setpoint, full
  power / silent);
- configure the device itself: WiFi SSID and password, DHCP or a static
  address, MQTT broker, and show what it is (IP, firmware, OTA slot);
- later, small graphs of the last hours.

The vendor has shown interest in the controller, which makes a real PCB - not
a devkit in a box - worth specifying.

Constraints and prior decisions:

- **ADR 0001**: Rust on the ESP32-C6, the bus behaviour of the stock controller,
  MQTT with HA discovery. The panel does not change the MCU or the bus logic.
- **ADR 0002**: OTA with a rollback bootloader. The bus gap across a planned
  reboot is kept small by running one bus exchange before the radio starts;
  an attempt to move the bus task to an interrupt executor crashed in esp-rtos
  and was rejected. Whatever the panel adds runs at the same, cooperative
  priority as the rest and must not delay the bus.
- **ADR 0003/0004**: the web UI, admin password, setup AP, settings records in
  the `nvs` sector (`fw/src/settings.rs`), BOOT (GPIO9) held 10 s while
  running = factory reset. USB is trusted because it needs physical access;
  the panel is physical access too.
- **The engraver** (`../stm32-engraver`): an ST7789 SPI display and an EC11
  encoder with push, driven from Rust with mipidsi and embedded-graphics, with
  a "repaint only changed fields" renderer (`Firmware/graver-controller/src/ui.rs`).
  Its lessons on the module pinouts and the backlight pin carry over.
- The firmware's command path already exists: `master::COMMANDS` (channel of
  `hp_model::Command`), `master::OUTCOMES` and `master::SNAPSHOT` (watches,
  8 subscribers, `fw/src/master.rs`). MQTT and the line ports use it; the
  panel is one more client.

---

## Decision

One PCB that is the whole panel and plugs into the stock controller's cable in
its place: ESP32-C6 module, powered from the cable's 12 V, a non-isolated 3V3
RS-485 transceiver, a **2.0" 240x320 ST7789V IPS display**, one EC11 rotary
encoder with push, USB-C for the first flash and as a bench supply. The
firmware gains a `panel` module behind a cargo feature; the DevKitC build
stays as it is.

### Hardware

| Block | Choice |
|---|---|
| MCU | ESP32-C6-WROOM-1 module (PCB antenna, antenna end over the board edge with the module's keep-out). WROOM-1U (U.FL) as a footprint-compatible option if the mounting place is a metal box. |
| Power in | 12 V from the cable through a PTC fuse, reverse-polarity protection (P-FET or Schottky) and a TVS sized for the measured rail. Diode-OR with USB-C VBUS. One synchronous buck, input range up to at least 28 V, 3V3 at >= 600 mA (WiFi peaks ~350 mA). |
| VIN sense | Divider into an ADC1 pin, shown on the Info screen: the 12 V budget is an open question and the board should measure it in service. |
| RS-485 | 3V3 half-duplex transceiver with failsafe receivers and high ESD rating (THVD1410 class), DE from a GPIO driven by the UART's RS-485 mode (RTS as DE). SM712 TVS on A/B. 120 R termination behind an open solder jumper. Not isolated: the board is powered from the heat pump's own rail and shares its ground, as the stock controller does. |
| Display | 2.0" 240x320 ST7789V IPS SPI module, 3.3 V, 8 pins (typically GND VCC SCL SDA RES DC CS BLK - verify on the bought module, the order varies between sellers). On a 1x8 2.54 mm header, front side. BLK driven from an LEDC PWM pin; check whether the module pulls BLK up (engraver note: float = on). |
| Input | EC11 encoder, 20 mm shaft, vertical, front side, through the front plate. Decoded by the C6's PCNT peripheral; push on a plain GPIO. External 10 k pull-ups and RC on A/B/SW. |
| Status | One WS2812 on GPIO8 as on the DevKitC, so `fw/src/led.rs` runs unchanged. |
| Service | USB-C to the C6's native USB-JTAG (GPIO12/13) with ESD protection; BOOT (GPIO9) and RESET tact switches reachable through the back; UART0 (GPIO16/17) on test pads. |
| Bus connector | Fits the stock controller's plug (type unknown, see Open Questions). Until known: a 4-pole 5.08 mm screw terminal footprint in parallel. |
| Form | 86 x 86 mm, the EU wall-box size of a typical wired controller; display upper part, encoder below, mounting holes on the 60 mm wall-box pattern. |

Proposed pin map (check against the WROOM-1 datasheet before layout; it is a
proposal, not verified):

| GPIO | Use | Note |
|---|---|---|
| 4 | UART1 RX (RO) | as today |
| 5 | UART1 TX (DI) | as today |
| 3 | RS-485 DE/RE | UART RTS in RS-485 mode |
| 6 | SPI2 SCK | FSPICLK IO_MUX pin |
| 7 | SPI2 MOSI | FSPID IO_MUX pin |
| 18 | display CS | |
| 19 | display DC | |
| 20 | display RES | |
| 21 | display BLK | LEDC PWM |
| 0 | encoder A | PCNT |
| 1 | encoder B | PCNT |
| 23 | encoder push | |
| 2 | VIN sense | ADC1 |
| 8 | WS2812 | strapping pin, same use as the DevKitC |
| 9 | BOOT button | strapping pin, ADR 0004 factory reset |
| 12/13 | USB D-/D+ | |
| 15 | strapping, unused | keep the default pull |
| 16/17 | UART0 | test pads |
| 10, 11, 22 | spare | e.g. a buzzer footprint for fault alarms (DNP) |

GPIO4 and GPIO5 are also strapping pins (MTMS/MTDI) on the C6. They are kept
because the firmware and the DevKitC wiring use them; the transceiver's RO
must not hold GPIO4 at a level that matters at reset - verify against the
datasheet's strapping table during schematic capture.

### Firmware

A `panel` cargo feature adds `fw/src/panel/`:

- **Display task**: mipidsi ST7789 over SPI2 with DMA, 240x320 portrait. The
  engraver's renderer pattern: a screen is a list of fields, and only fields
  whose content changed are repainted. A full frame (150 kB) is never pushed
  in one go; graphs are drawn as horizontal strips into a small buffer. The
  task yields between strips so a pending bus slot is never held up by more
  than one strip.
- **Input**: PCNT counts encoder edges in hardware; a small task turns counts
  into detents and the push into short press / long press (1 s). The encoder
  wakes the backlight; the first turn after wake-up only wakes.
- **State**: subscribes `SNAPSHOT` (one of the 8 subscriber slots), sends
  `Command`s into `COMMANDS`, subscribes `OUTCOMES` around a write and shows
  the result ("applied", "rejected: out of range", "no answer"). No new path
  to the bus.
- **Menu model** in a small host-testable crate (`panel-ui`, `no_std`, like
  `hp-model`): screens, cursor, edit state, value ranges taken from
  `hp_model`'s entity table. Rendering stays in the firmware. Host tests cover
  navigation and range clamping; an embedded-graphics simulator target is
  optional.

Screens:

1. **Home** (default, returns after 60 s idle): inlet and outlet water
   temperature, active setpoint, mode, on/off, compressor frequency, and a red
   fault banner when a fault bit is set. A status line: WiFi, MQTT, bus
   (link/timeouts), master/listen.
2. **Set** (short press from Home): power, mode (heat/cool/auto), the setpoint
   for the current mode, full power / silent, stop-at-target (P05). Turn to
   pick, press to edit, turn to change, press to send. Each send is one
   `Command`; the screen waits for its outcome.
3. **Settings** (long press from Home):
   - **WiFi**: a scan list to pick the SSID; the password is entered on a
     phone through ADR 0004's setup AP, which the panel opens on request and
     shows as two QR codes (`WIFI:T:nopass;S:wfi028t-setup-XXXX;;` to join,
     then the setup URL). A rotary character picker is the fallback for
     entering it on the panel itself.
   - **Address**: DHCP or static - address, netmask, gateway, DNS, set one
     octet at a time.
   - **MQTT**: host, port, on/off (user and password through the web page).
   - **Heat pump**: slave address (1-16), mode master/listen with the same
     warning and silence check the web wizard uses.
   - **Display**: brightness, backlight timeout, panel lock PIN.
   - **Factory reset**: hold the knob 5 s on a confirmation screen; same
     effect as BOOT held 10 s (ADR 0004).
4. **Info**: IP, MAC, hostname, firmware version and commit, OTA slot and
   state, uptime, bus counters, VIN.
5. **Graphs** (later, not in the first build): the last 24 h of inlet/outlet
   temperature, setpoint and compressor frequency, one sample per minute in a
   RAM ring (4 values x 1440 x 2 B = 11.5 KiB), lost on reboot.

Settings records (same sector as ADR 0004, new offsets after its records,
magic + version + CRC like the others):

| Record | Bytes | Content |
|---|---|---|
| net | 32 | DHCP/static flag, address, netmask, gateway, DNS |
| panel | 32 | brightness, backlight timeout, lock PIN (salted hash), flags |

The **net** record is not panel-specific: the web UI's WiFi page (ADR 0004)
gets the same DHCP/static fields, and `net.rs` applies it with embassy-net's
static configuration instead of DHCPv4.

Trust: the panel is physical access and gets USB's level - every setting. The
optional lock PIN (off by default) guards Set and Settings against casual
changes by others at the pool; it is not a security boundary (whoever holds
the board can read flash, or hold BOOT).

---

## Architecture Overview

### Component Breakdown

1. **Panel PCB** (`hw/panel/`, new KiCad project)
   - Power, MCU, RS-485, display header, encoder, USB-C, as in the table.
   - A parts table with LCSC numbers first (as the engraver's
     `docs/parts-*.md`), then the schematic, then layout for JLC assembly of
     the SMD side; the encoder and display header are THT.
2. **Display and input** (`fw/src/panel/display.rs`, `input.rs`, new)
   - SPI2 + DMA, mipidsi, embedded-graphics, profont.
   - PCNT unit for the encoder, GPIO for push, LEDC for the backlight.
3. **Menu model** (`panel-ui/`, new crate in the workspace)
   - Pure state machine: `Event` (turn, press, long press, snapshot, outcome)
     in, `View` (what each field shows) out. No I/O.
4. **Settings** (`fw/src/settings.rs`)
   - `net` and `panel` records; factory reset erases both.
5. **Network** (`fw/src/net.rs`)
   - Static configuration from the `net` record; scan results exposed to the
     panel as they are to the web page.
6. **Existing, unchanged**: `master.rs` (command path), `poll.rs` (bus),
   `mqtt.rs`, `ota.rs`, `led.rs`.

### Data Flow / Interaction

```
             +-----------+   SNAPSHOT (watch)   +-----------+
  RS-485 <-> | poll.rs / | -------------------> | panel     | --> SPI2 DMA --> ST7789V
             | master.rs | <------------------- | (menu,    |
             +-----------+   COMMANDS (chan)    |  render)  | <-- PCNT / GPIO <-- EC11
                   |        OUTCOMES (watch)    +-----------+
                   |                                  |
                   +--> mqtt.rs, line ports           +--> settings.rs (net, panel,
                        (same three statics)               wifi, mqtt records)
                                                      +--> net.rs (scan, static IP,
                                                           open setup AP)
```

---

## Alternatives Considered

### 1.3" 240x240 OT3499 (the engraver's module)
- **The idea**: reuse the engraver's display and code verbatim.
- **Optimizes for**: zero new parts, spares on hand, the code is written.
- **Sharpest tradeoff**: 57,600 pixels leave no room for a graph next to the
  values; the user asked for room for graphs.
- **Bets on**: the panel staying a value readout forever.

### 3.5" 480x320 (ST7796)
- **The idea**: the biggest common SPI module, same wiring.
- **Optimizes for**: graphs and readability from a distance.
- **Sharpest tradeoff**: 4x the pixels of 2.0" per full repaint, 300 kB frames,
  and a board larger than the wall-box format.
- **Bets on**: graphs becoming the main use of the panel. If that happens, the
  board revision is mostly the header footprint and the size constants.

### Touch screen (2.8" with resistive or capacitive touch)
- **The idea**: an on-screen keyboard for the WiFi password.
- **Optimizes for**: text entry.
- **Sharpest tradeoff**: wet hands at the pool, glare, a touch controller and
  a widget toolkit in firmware, for a password typed once.
- **Bets on**: configuration being the panel's main job, and no phone at hand.

### No screen; the web UI is the panel
- **The idea**: ADR 0003/0004 already give a page on any phone.
- **Optimizes for**: no hardware work.
- **Sharpest tradeoff**: nothing to see at the heat pump; a WiFi failure leaves
  no way to read the state.
- **Bets on**: nobody looking at the heat pump without a phone.

### Carrier board for the DevKitC
- **The idea**: plug the existing DevKitC into a board with the display,
  encoder, transceiver and buck.
- **Optimizes for**: no RF layout, fastest first unit.
- **Sharpest tradeoff**: larger, the devkit's own connectors and LED in the
  way, not something to hand to a vendor.
- **Bets on**: only one or two units ever being built.

### Isolated RS-485 (as the current module)
- **The idea**: keep galvanic isolation.
- **Optimizes for**: ground faults between two separately powered ends.
- **Sharpest tradeoff**: an isolated DC/DC and transceiver for a board powered
  by, and grounded to, the same heat pump.
- **Bets on**: the board being powered from somewhere else than the cable. If
  the 12 V rail turns out unusable and the board runs from a separate supply,
  this alternative comes back.

---

## Consequences

### Positive
- The device becomes a drop-in for the stock controller: same plug, same
  place, no extra power supply.
- The stock controller can no longer be on the bus at the same time by
  accident: they share the plug.
- State and faults are visible at the heat pump, also without WiFi.
- First setup works with only the panel and a phone camera (QR).
- The static IP setting lands for the web UI too.

### Negative
- A real RF, power and EMC design to get right, and a board revision cycle.
- Another task on the single cooperative executor next to the bus.
- Two front-ends for the same settings: every new setting has to be placed
  on the panel or deliberately left web-only.

### Risks
- **The 12 V rail cannot supply the board** (WiFi peaks). Mitigation: measure
  first; USB-C supply as fallback; VIN on the Info screen.
- **Display work delays the bus.** Mitigation: strips, yield between them,
  DMA; measure the poll timing (`poll.rs` counters, the capture server's
  frame times) with the panel on and repainting continuously.
- **Strapping pins** (4, 5, 8, 9, 15) pulled the wrong way by the
  transceiver or the LED at reset. Mitigation: check every strapping pin's
  external level in schematic review.
- **WiFi reach at the mounting place** with a PCB antenna near a wall box or
  a metal enclosure. Mitigation: antenna over the board edge, WROOM-1U option.
- **The display module's pinout or panel offset differs** between sellers.
  Mitigation: buy the modules first, design the footprint from one in hand.

---

## What an Expert Would Ask

**Q: The bus task and the display share one cooperative executor. What is the
worst-case delay the panel can add to a bus exchange, and how is it proven?**
A: The bus runs in 500 ms slots and the heat pump answers in about 150 ms, so
the timing budget is generous; reception is FIFO/interrupt-driven and does not
lose bytes while a task runs. The risk is a long non-yielding section:
font rendering of a full screen, or a QR encode. The rule is no section longer
than 5 ms without an await, and the proof is a measurement, not an argument:
poll period and response-to-next-request jitter from the capture server with
the panel repainting continuously. If that fails, the fallback is not another
executor (ADR 0002 showed that crash) but smaller strips and a precomputed QR.

**Q: Is the 12 V on the cable really there for us, and what if it sags?**
A: Unknown. The stock controller runs from it, with a display and backlight,
so a few hundred mA at 12 V is plausible but not proven. It must be measured
(voltage under load, and the stock controller's draw) before the power stage
is fixed. A board that browns out during WiFi transmit would reset in a loop,
and every reset is a bus gap. The buck's input range and the VIN sense give
room and visibility; USB-C power is the fallback.

**Q: Without isolation, what happens on a ground shift or a surge on a long
cable outdoors?**
A: Both ends are the same heat pump's ground and supply, so there is no ground
shift in normal service; that is also how the stock controller is built. Surges
are handled by the TVS on A/B and on the 12 V input, and the transceiver's
ESD rating. A lightning-class surge is not handled, as it is not by the
stock controller.

**Q: Can somebody at the pool change settings, or lock the owner out?**
A: Yes, by design they can change setpoints - like the stock controller. The
optional lock PIN stops casual changes; it is not protection against someone
with a screwdriver. Locking out is not possible: BOOT held 10 s and USB always
work, and a forgotten PIN is cleared by factory reset.

**Q: Two front-ends edit the same records. What if the web page and the panel
change the same setting at the same time?**
A: Records are written whole through `settings.rs` under its mutex, so the
last writer wins and the sector is never half-written. Heat pump settings go
through `COMMANDS`, one per cycle, each against the latest readback, so two
setpoint changes simply apply in order. The panel re-reads after each outcome,
so it never shows a stale value as current.

---

## Implementation Plan

### Decisions you will probably want to tweak

- **Display size**
  - **Choice**: 2.0" 240x320 ST7789V.
  - **Alternative**: 3.5" 480x320 ST7796.
  - **Cost to change later**: a header footprint, board outline and size
    constants; the renderer is resolution-independent if fields are laid
    out in a table, not at hard-coded pixels.
- **Encoder, no keys**
  - **Choice**: one EC11 with push.
  - **Alternative**: three keys (up, down, OK), or encoder plus a "back" key.
  - **Cost to change later**: a footprint and the input mapping; the menu
    model takes events, not pins.
- **Power from the cable**
  - **Choice**: 12 V from red/black, USB-C diode-ORed.
  - **Alternative**: separate 5 V supply.
  - **Cost to change later**: high once boards are made - the power stage and
    connector are the board.
- **Non-isolated RS-485**
  - **Choice**: THVD1410 class, shared ground.
  - **Alternative**: isolated transceiver plus isolated DC/DC.
  - **Cost to change later**: a board revision.
- **net record shared with the web UI**
  - **Choice**: one record, both front-ends.
  - **Alternative**: panel-only static IP.
  - **Cost to change later**: low before release; a record version bump after.

### Known unknowns and how the plan absorbs them

- **Connector type**: default a 5.08 mm screw terminal footprint; pivot when a
  photo of the stock plug identifies it.
- **12 V budget**: default a buck rated to 28 V input and >= 600 mA out; pivot
  to a separate supply (and reconsider isolation) if the rail sags below
  ~9 V under the stock controller's own load.
- **Mounting place**: default indoors, PCB antenna; pivot to WROOM-1U and a
  sealed enclosure with conformal coating if it sits outside on the unit.
- **Display module pinout and offset**: default GND VCC SCL SDA RES DC CS BLK,
  offset 0/0; pivot to whatever the module in hand shows.
- **Termination**: default open jumper; pivot if the stock controller is found
  to terminate the line (measure across A/B on its plug, unpowered).

### The mechanical work

- `hw/panel/`: parts table with LCSC numbers and stock, schematic (power, MCU,
  RS-485, display, encoder, USB), ERC, layout, DRC, JLC fab and assembly files.
- `panel-ui` crate: state machine and host tests.
- `fw/src/panel/`: display, input, backlight, screens; `panel` feature; a
  board config for the pin map next to the DevKitC one.
- `settings.rs`: `net` and `panel` records with tests for an old sector
  (records absent -> DHCP, defaults).
- `net.rs`: static configuration; scan list shared with the web page.
- README: the panel as the hardware option next to DevKitC + module.

Review asks:

1. Power the board from the cable's 12 V, with USB-C as fallback - yes/no?
2. Non-isolated RS-485 transceiver - yes/no?
3. Lock PIN off by default - yes/no?
4. Static IP as one record shared with the web UI (ADR 0004's WiFi page gains
   the fields) - yes/no?

---

## Open Questions

**Architecture-changers**
- [ ] What connector is on the controller cable at the stock controller?
      A photo of the plug and of the stock controller's board connector.
- [ ] What does the 12 V rail deliver: voltage at idle and under the stock
      controller's load, and what does the stock controller draw?
- [ ] Where is the panel mounted: indoors (pool house, wall box) or on the
      unit outdoors? Decides the enclosure, the antenna and the coating.

**Behavior definers**
- [ ] Is black (12 V return) the same ground as the bus reference? Check
      continuity to the heat pump's RS-485 ground before relying on it.
- [ ] Does the stock controller terminate the bus? Decides the jumper default.
- [ ] Which settings are panel-editable and which stay web-only (MQTT
      credentials, admin password, line-port policy are web-only here).
- [ ] Fault alarm: a buzzer, or only the red banner and the backlight?
