# First run: setup access point, stored WiFi, heat pump discovery

**Status**: Proposed
**Date**: 2026-10-04
**Updated**: 2026-10-06 (vendor protocol: the slave address is 1-16 by DIP
switch, and writes get no exception response; discovery adapts)

---

## Context

Getting a controller running today takes a developer:

- WiFi credentials are compiled in (`SNIFFER_WIFI_SSID` / `SNIFFER_WIFI_PASS`,
  [`fw/src/main.rs`](../../fw/src/main.rs)); a different network is a rebuild.
- MQTT can be set at runtime, but only from a console (`mqtt host ...`,
  [`fw/src/cmd.rs`](../../fw/src/cmd.rs)).
- The bus settings (9600 8N1) and the switch to master mode are console
  commands, and knowing when master mode is safe - stock controller unplugged,
  bus silent, slave 0x01 answering - is knowledge from the bring-up
  (ADR 0001, `docs/register-map.md`), not something the device guides anyone
  through.

The goal is the "cherry on the cake": a repository where someone with the
board can build (or download), flash, boot, open a web page, configure, and
run - without a toolchain change, a console, or reading the bring-up notes.

What already exists and is reused:

- The settings sector in the `nvs` partition
  ([`fw/src/settings.rs`](../../fw/src/settings.rs)): CRC'd records for bus,
  mode and MQTT, one 4 KiB sector, rewritten whole.
- Listen mode, the 3 s silence check and the classification of every frame
  ([`fw/src/poll.rs`](../../fw/src/poll.rs)): the bus side of discovery is
  mostly there.
- `hp_model` decodes both register blocks; the entity table knows the legal
  ranges.
- ADR 0003: the web server, the guard, the admin password and sessions. This
  ADR adds the setup mode and the pages that sit on it.
- esp-radio supports access point, station, and both at once
  (`Config::AccessPointStation`), and network scans (`scan_async`).

Constraints:

- **Never transmit by surprise.** Until someone has confirmed the takeover in
  the wizard, the bus stays in listen mode, whatever else happens. Setup,
  WiFi trouble and factory reset all fall back to listen.
- **Setup must not be a standing open door.** An open access point that hands
  the device to whoever joins first is acceptable only while the device is
  unconfigured or someone has physically asked for it.
- **GPIO9 (BOOT) is a strapping pin**: held low at reset the chip enters ROM
  download mode. A button-at-power-on gesture is not available; the gesture
  has to happen while running.
- **RAM**: the AP side needs a second network interface, a DHCP server and a
  DNS responder. Budget: well under 20 KiB.

---

## Decision

Two modes, chosen at boot and switchable at runtime:

- **Setup mode**: the device runs an access point `wfi028t-setup-XXXX` (last
  four hex digits of the MAC), open, at 192.168.4.1, with a DHCP server and a
  DNS responder that answers every name with 192.168.4.1, so phones show the
  setup page as a captive portal. The station side runs too
  (`AccessPointStation`), so the WiFi step can test the home network while the
  phone stays on the setup network.
- **Normal mode**: station only, as today.

Setup mode is entered when, and only when:

- the device is unconfigured (no admin password record), or
- the BOOT button was held for 10 s while running (factory reset), or
- the logged-in settings page asks for it ("re-run setup"), or
- the configured network has not been reached for 10 minutes since boot
  (WiFi lost or changed). This fallback runs the AP alongside the station,
  keeps the existing configuration, and closes the AP as soon as the station
  connects. It requires the admin password on the setup page - it is not a
  reset.

An unconfigured device's AP closes after 30 minutes without a completed setup
and reopens at the next power cycle, so a forgotten device does not advertise
an open door indefinitely.

The setup flow is a wizard of five pages served by ADR 0003's server:

1. **Admin password** (twice). Stored as ADR 0003 specifies. From here on the
   setup session is a logged-in session.
2. **WiFi**: scan list (SSID, signal, security) plus a manual entry; password;
   "connect" tests it live on the station side and shows the address it got.
3. **MQTT** (skippable): host, port, user, password; "test" opens a connection
   and waits for CONNACK. Home Assistant discovery is on.
4. **Heat pump**: the discovery wizard below. Skippable: the device then runs
   in listen mode, and the wizard is reachable later from the settings page.
5. **Done**: saves, shows the address in the home network
   (`http://<ip>/` and `http://wfi028t-XXXX.local/`), closes the AP after
   60 s.

### Heat pump discovery

A state machine in the firmware, driven and shown by the page. It reads, and
transmits only reads, and only after the bus has been proven to have no other
master. Steps:

1. **Listen** with the stock controller still connected (the safe state the
   board is installed in). For each candidate bus configuration - 9600 8N1
   first (the measured one), then 9600 8E1, 9600 8N2, 4800 8N1, 19200 8N1 -
   listen 3 s and count CRC-good frames, UART errors and raw bytes.
   - CRC-good frames with the stock controller's pattern (reads of
     0x0000 x 63 and 0x003f x 67, about once a second): configuration
     found, stock controller present, and the slave address is whatever it
     polls. The heat pump's answers
     are decoded passively and shown ("inlet 33 C, heating, P01 33") so the
     user sees it is the right machine before anything else happens.
   - Bytes but no valid frame on any configuration, or framing errors: "wiring
     - check A and B (swap them)". A/B swapped inverts the line, which shows
     up exactly as this.
   - No bytes at all on any configuration: either the stock controller is
     already unplugged, or nothing is connected. Go to step 3.
2. **Unplug the stock controller.** The page says so and waits until the bus
   has been silent for the 3 s silence check. Traffic resuming later aborts
   the wizard back to listen.
3. **Probe** (bus proven silent). With the found configuration, or each
   candidate in turn if none was found, send one read of the status block and
   wait for the answer. The address is the one the stock controller polled;
   without that, slave 0x01 first, then 2..16 (the vendor protocol sets it by
   DIP switches 1-4). The heat pump sends no exception responses, so a wrong
   address or configuration shows as a timeout, nothing more. No answer
   anywhere: "nothing answers - check power and wiring". One read per
   candidate, never a write.
4. **Check plausibility.** Read both blocks with `hp_model`; every decoded
   value must be in its physical or entity range (temperatures -30..100 C,
   setpoints within the entity table, a known mode). Show the summary. A
   failed check is shown, not hidden: "answers, but does not look like a
   WFI-028T" with the values.
5. **Take over.** One button, with the consequence spelled out ("the
   controller becomes the bus master; the stock controller must stay
   unplugged"). It saves the bus configuration and `mode master`, and runs
   the normal engage path with its silence check.

### Line ports

ADR 0003 left 4000/4001 LAN-trusted. Released images change that default:

| Setting | 4000/4001 accept |
|---|---|
| `full` | every command, as today (default for bench builds) |
| `read-only` | the stream and `status`; anything else answers `# err read-only` (default for released builds) |
| `off` | nothing; the ports are closed |

USB always gets `full`: it needs physical access. The setting lives on the
logged-in settings page and on the USB console. The line ports listen on the
station interface only, never on the setup AP.

---

## Architecture Overview

### Component Breakdown

1. **Settings records** ([`fw/src/settings.rs`](../../fw/src/settings.rs))
   - Same sector, new records at new offsets after the MQTT record; existing
     offsets do not move, so a controller updated in place keeps its bus,
     mode and MQTT settings (ADR 0002's promise holds).
   - New records, each with magic, version and CRC like the existing ones:

     | Record | Bytes | Content |
     |---|---|---|
     | WiFi | 112 | SSID (32), password (64), flags |
     | admin | 64 | PBKDF2 salt, iterations, hash (ADR 0003) |
     | device | 48 | hostname, line-port policy, setup-complete flag |

   - Build-time `SNIFFER_WIFI_*` and `MQTT_*` stay as defaults under a stored
     record, as MQTT already works: stored wins, `off` wins over both. A bench
     build with credentials therefore still boots straight onto the bench
     network; a released build has none.
   - WiFi and MQTT passwords are stored in clear: flash encryption is off, and
     anyone with the board in hand can read flash anyway. Never echoed back by
     any page or command.
   - Factory reset erases the WiFi, admin, MQTT and device records, sets mode
     to listen, and keeps the bus record (it describes the wiring, not the
     owner).

2. **Radio and interfaces** ([`fw/src/net.rs`](../../fw/src/net.rs))
   - Two embassy-net stacks: station (as today) and access point (192.168.4.1/24,
     static). Both are created at boot; the AP interface carries traffic only
     while the radio is in `AccessPointStation`.
   - A `wifi_task` state machine: `Station` <-> `SetupAndStation`, driven by the
     conditions in the Decision. Switching the radio configuration does not
     reboot and does not touch the bus.
   - Scan for the WiFi page via `scan_async` on the station side.
   - The first radio start keeps ADR 0002's ordering: after a planned reboot
     in master mode, one bus exchange first.

3. **Setup network services** ([`fw/src/setup/`](../../fw/src/setup), new)
   - `dhcp.rs`: a minimal DHCP server on the AP stack: DISCOVER/OFFER,
     REQUEST/ACK, 8 leases from 192.168.4.10, router and DNS = 192.168.4.1,
     one-hour lease. No relay, no options beyond those.
   - `dns.rs`: answers every A query with 192.168.4.1, `AAAA` with an empty
     answer, everything else with `NOTIMP`. One UDP socket.
   - Captive-portal probes (`/generate_204`, `/hotspot-detect.html`,
     `/connecttest.txt`, `/ncsi.txt`) on the AP interface answer `302` to
     `http://192.168.4.1/setup`. In setup mode the `Host` allowlist (ADR 0003)
     also accepts `192.168.4.1` and, for these probe paths only, any host.
   - mDNS responder on the station stack: answers A queries for
     `<hostname>.local` (default `wfi028t-XXXX`). Nothing else - no service
     records yet.

4. **Discovery** ([`fw/src/discover.rs`](../../fw/src/discover.rs), new)
   - Owned by the bus task, like `engage_master`: the bus task is the only
     code that touches the UART, and discovery runs between its slots in listen
     mode. A `DISCOVER_REQUEST` signal starts a step; a `Watch` publishes the
     progress (`step`, `config under test`, counts, decoded preview, verdict).
   - Uses `BusUart::apply` for configuration changes (as the `bus` command
     does), the existing frame classifier for "another master", the existing
     silence check, and `hp_model::rtu` for the probe and the decoding.
   - Never sends anything but a status-block read, and only after a silence
     check on the configuration under test.
   - Ends by restoring the saved bus configuration unless the user takes
     over.

5. **Setup and settings pages** ([`fw/web/`](../../fw/web))
   - `setup.html`: the five-step wizard; each step is one form and one
     endpoint, so a phone that loses the page can resume at the step the
     device reports.
   - `settings.html` (logged in, normal mode): WiFi, MQTT, hostname, line-port
     policy, password change, "run the heat pump wizard", "listen / master"
     with a confirm, "re-run setup".
   - Endpoints under `/api/settings/` and `/api/setup/` (ADR 0003's server and
     guard): `password`, `wifi/scan`, `wifi`, `wifi/test`, `mqtt`, `mqtt/test`,
     `device`, `discover/start`, `discover/state`, `discover/takeover`,
     `mode`, `setup/finish`. Each `POST` validates fully and answers the new
     state; nothing is half-applied.

6. **Factory reset** ([`fw/src/button.rs`](../../fw/src/button.rs), new)
   - GPIO9 with pull-up, sampled every 100 ms by a small task. Held 10 s:
     the LED turns red, the bus drops to listen, the records are erased, and
     the device reboots into setup mode.
   - The press is acted on only after the full 10 s; a shorter press does
     nothing. A press at power-on is ROM download mode, documented, not
     handled.

7. **LED** ([`fw/src/led.rs`](../../fw/src/led.rs))
   - Gains two states: setup mode (slow blue pulse) and factory reset pending
     (red, while the button is held past 3 s).

8. **`status` and docs**
   - `status` gains `setup=off|ap|ap+sta`, `configured=yes|no`,
     `line_ports=full|read-only|off`.
   - README: "first run" at the top: flash, join the setup network, the
     wizard, factory reset. The bring-up notes move below it.

### Data Flow / Interaction

```
power on
  |
  +-- admin record? --no--> setup mode: AP + STA, bus in listen
  |                           phone joins wfi028t-setup-XXXX
  |                           DHCP 192.168.4.x, DNS -> 192.168.4.1
  |                           OS captive probe -> 302 /setup
  |                           wizard: password -> WiFi (test on STA)
  |                                   -> MQTT (test) -> heat pump -> done
  |                           AP closes 60 s after done
  |
  +-- yes --> normal mode: STA only
                no connection for 10 min --> AP + STA (password required)
                BOOT held 10 s ----------> erase, listen, reboot to setup

heat pump step (bus task):
  listen per config --> stock controller traffic? --> "unplug it" --> silence
                    --> nothing at all --------------------------> silence
  silence --> probe read slave 1..16 per config --> plausible? --> take over
```

---

## Alternatives Considered

### Separate setup firmware (or a setup mode only reachable by reflashing)
- **The idea**: the controller image has no AP code; setup is done by a
  different image or over USB.
- **Optimizes for**: the smallest attack surface in the running image.
- **Sharpest tradeoff**: "flash, boot, configure" becomes "flash, configure,
  flash again", and changing WiFi later needs a cable.
- **Bets on**: users having a PC with the toolchain at hand. That is exactly
  what this ADR is trying to stop requiring.

### Improv WiFi over serial / BLE provisioning
- **The idea**: the ESP Web Tools flasher sends WiFi credentials over USB
  right after flashing (Improv), or a phone app does it over BLE.
- **Optimizes for**: no AP, no captive portal code.
- **Sharpest tradeoff**: Improv over serial only works at flash time from the
  browser flasher; BLE needs an app and a BLE stack sharing the radio and RAM.
  Neither covers "the WiFi changed" months later.
- **Bets on**: everyone flashing from the browser. Worth adding later as a
  shortcut on top of the AP (ADR 0005), not instead of it.

### WPA2-protected setup AP with a per-device password
- **The idea**: the setup network has a password, derived from the MAC and
  shown on the USB console or a label.
- **Optimizes for**: no window in which a neighbour could claim the device.
- **Sharpest tradeoff**: there is no label and no display; the password would
  live in a console log nobody reads, or be guessable from the MAC that the
  AP broadcasts anyway.
- **Bets on**: a printed label. If the project ships boards (ADR 0005), this
  revives with a sticker.

### Fully automatic takeover
- **The idea**: discovery switches to master mode by itself once the bus is
  silent and the heat pump answers.
- **Optimizes for**: zero clicks.
- **Sharpest tradeoff**: the device starts transmitting on someone's heat pump
  because a cable came loose for 3 s.
- **Bets on**: silence always meaning "unplugged on purpose". It does not.

### Probe every configuration with the stock controller still connected
- **The idea**: skip the passive listen; send reads and see which
  configuration answers.
- **Optimizes for**: speed.
- **Sharpest tradeoff**: two masters on one bus - the one thing ADR 0001 is
  built to never do.
- **Bets on**: nothing acceptable. Rejected.

---

## Consequences

### Positive
- Flash, boot, configure from a phone, run: no toolchain change, no console,
  no rebuild for a different network.
- The bring-up knowledge (unplug first, silence, slave address, plausibility)
  becomes a guided procedure that refuses the unsafe orders.
- One image fits every installation, which is what makes released binaries
  (ADR 0005) possible.
- Losing WiFi or the password is recoverable without a cable.

### Negative
- A second network interface, a DHCP server, a DNS responder and an mDNS
  responder to own and keep small.
- The open setup AP is a window: whoever joins first while the device is
  unconfigured owns it.
- WiFi and MQTT passwords sit in flash in clear.
- More pages, more endpoints, more to test; the web guard (ADR 0003) gets a
  setup-mode exception for captive probes.

### Risks
- **Neighbour claims an unconfigured device.** Mitigation: the window exists
  only while unconfigured or after a deliberate reset, closes after 30
  minutes, and the bus is in listen mode throughout; the owner sees it at
  once (the wizard is already done) and resets with the button.
- **AP + STA on one radio**: both must share the station's channel; a home
  network on another channel moves the AP, and a phone may drop off the
  setup network during the WiFi test. Mitigation: the WiFi step reports the
  result on the station side and the page re-polls; the device keeps the
  result for the next page load.
- **Discovery misjudges the bus.** A stock controller that polls less often
  than once per 3 s window, or pauses, could look like silence. Mitigation:
  the silence check before every probe is the existing one; the page still
  asks the user to confirm the stock controller is unplugged; any foreign
  frame later drops to listen (existing behaviour).
- **Captive portal quirks** across Android/iOS/Windows. Mitigation: the setup
  page is also reachable by typing `192.168.4.1`, said on the AP's first
  screen in the README.

---

## What an Expert Would Ask

**Q: Someone sets up the device and leaves the open AP within range of the
street. What can a stranger do?**
A: After setup completes, nothing: the AP closes. It only reopens in two ways
without the password: a factory reset (physical button) or a power cycle of a
device that was never configured. The fallback AP after a WiFi loss requires
the existing admin password. During an unconfigured window a stranger could
claim the device - but there is no heat pump control until someone confirms
a takeover, and the owner notices at the next page load.

**Q: The heat pump firmware polls once a second. What if the stock
controller is off at the moment of the listen step, and comes back after
takeover?**
A: Then there are two masters, and the existing classifier sees a frame that
is neither our echo nor a slave answer and drops to listen within one slot
(ADR 0001, `poll.rs`). Discovery adds the user's confirmation; the bus task's
rule is the backstop.

**Q: Why not store the WiFi password encrypted?**
A: With what key? Flash encryption is off (enabling it is irreversible on the
C6 and complicates every USB reflash), and a key in the same flash protects
nothing. The honest statement is in the README: physical access to the board
gives the WiFi password.

**Q: Two stacks, a DHCP server, DNS, mDNS - how much RAM, and what happens
to the bus during a WiFi scan?**
A: Estimate: AP stack resources and three sockets ~10 KiB, DHCP/DNS buffers
~3 KiB, mDNS ~2 KiB. To be measured on the bench before the pages are
written. A scan blocks the radio for ~2 s but not the executor in the way the
first radio start does; the mock bench measures it the same way as ADR 0002's
gap (frame-to-frame silence) before the WiFi page ships.

**Q: A controller already in the field (the unit at the heat pump) gets this
image by OTA. Does it fall into setup mode and stop polling?**
A: No admin record -> "unconfigured" -> setup mode, yes - but setup mode does
not touch the bus: the stored mode (master) and bus settings still apply, so
it keeps polling. A build that has `SNIFFER_WIFI_*` set keeps using those
credentials on the station side while the AP is up, so Home Assistant keeps
working. The first person to open the setup page sets the password. This is
the upgrade path, and it is tested on the bench board before the live unit.

---

## Implementation Plan

### Decisions you will probably want to tweak

- **Open setup AP, time-limited.**
  - Choice: open, unconfigured-only or after a button reset, 30 min.
  - Alternative: WPA2 with a MAC-derived password.
  - Cost to change later: small in code; needs a way to show the password.
- **Discovery candidates.**
  - Choice: 9600 8N1, 8E1, 8N2, 4800 8N1, 19200 8N1; slave 1..16, 0x01
    first.
  - Alternative: 9600 8N1 only (the vendor protocol fixes it), or the full
    baud/parity matrix.
  - Cost to change later: a table; each extra candidate adds 3 s to listen.
- **Factory reset keeps the bus record.**
  - Choice: keep wiring, forget the owner.
  - Alternative: erase everything.
  - Cost to change later: one line.
- **Hostname `wfi028t-XXXX`** (was `wfi-controller`).
  - Choice: MAC suffix, so two units on one network do not collide.
  - Alternative: keep `wfi-controller` as the default.
  - Cost to change later: the capture service, docs and bookmarks follow the
    name. The live unit keeps `wfi-controller` by setting it explicitly.
- **Line ports read-only by default in released builds.**
  - Choice: as in the Decision.
  - Alternative: full, as today.
  - Cost to change later: one default; bench workflows depend on `full`.

### Known unknowns and how the plan absorbs them

- **AP + STA stability on esp-radio 1.0.0-beta.1**: default AP+STA for setup.
  Pivot if the radio misbehaves: AP-only setup with a reboot into station
  mode to test the network, the result shown at the next setup visit.
- **Silence windows of the stock controller**: default 3 s, as the existing
  check. Pivot if the bench or the real unit shows longer pauses in the stock
  controller's polling: lengthen the listen window per candidate.
- **Captive-portal detection paths**: default the four known probes. Pivot:
  add whatever a tested phone asks for; the fallback is typing the IP.
- **RAM**: default both stacks always present. Pivot if the measured cost is
  too high: create the AP stack only in setup mode and reboot (planned, with
  the marker) to leave it.

### The mechanical work

- `settings.rs`: three records, factory-reset erase, tests for old-sector
  compatibility (an existing sector must read back unchanged).
- `net.rs`: AP stack, radio mode state machine, scan.
- `setup/dhcp.rs`, `setup/dns.rs`, `setup/mdns.rs`: small, host-tested
  parsers and responders.
- `discover.rs` inside the bus task; mock-bench scenarios: stock-controller
  traffic (a second mock as master), silent bus, swapped A/B (inverted
  adapter), wrong baud, implausible slave.
- `button.rs`, LED states.
- Pages and endpoints on ADR 0003's server.
- README "first run"; ADR 0001/0002 cross references.

Review asks:

1. Open setup AP, only while unconfigured or after a button reset, 30-minute
   window - yes, or WPA2 with a MAC-derived password?
2. Factory reset keeps the bus record - yes/no?
3. Hostname default `wfi028t-XXXX` for new devices (the live unit keeps
   `wfi-controller` explicitly) - yes/no?
4. Released builds default the line ports to read-only - yes/no? (Same as ADR
   0003 review ask 3.)

---

## Open Questions

**Architecture-changers**
- [ ] AP+STA at once, or AP-only setup with a reboot to test the network?
      Decided by a bench test of esp-radio's AP+STA before the WiFi page.
- [ ] Does the upgrade path for the live unit (OTA into an image with setup
      mode) need a "configured by build" flag, so it never shows an open AP?

**Behavior definers**
- [ ] Discovery: should it also offer to read the heat pump's settings into a
      backup file the user downloads, before takeover?
- [ ] After a factory reset, should the device keep polling in master mode if
      it was master (the heat pump keeps running), or always drop to listen
      (safer, but the heat pump raises E09)? Current choice: listen.
