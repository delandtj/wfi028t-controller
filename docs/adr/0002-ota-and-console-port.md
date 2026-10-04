# OTA firmware updates and a second TCP client slot

**Status**: Accepted
**Date**: 2026-10-04
**Updated**: 2026-10-04 (review asks settled: commands pass during probation, header format as specified, separate ports 4001/4002, key at ~/.config/wfi028t/)

---

## Context

The firmware (ADR 0001) runs the heat pump as bus master since 2026-10-04.
Two gaps showed up on the first day:

- **Updates need USB.** The flash holds one app partition (`factory`, espflash
  default table) and nothing can replace it over the network. The board will
  live at the heat pump, away from a PC. ADR 0001 component 5 left OTA open
  ("crate choice open").
- **Port 4000 has one client.** `fw/src/net.rs` serves one connection at a
  time, newest wins, with ONE cursor into the line ring that survives across
  clients (that is what makes the replay work). The capture daemon
  (`wfi-controller-capture.service`) now holds that connection permanently.
  Any other client knocks it off, and every line delivered to that client is
  never written to the capture files. `status` and `note` go around this
  through the daemon's control socket, but `set`, `mode` and `mqtt` have no
  network path that leaves the log intact.

Constraints and facts:

- ESP32-C6, 16 MB flash, image today ~755 KB.
- `esp-bootloader-esp-idf` 0.6 (already a dependency) has `OtaUpdater`
  (write the next app partition, select it in `otadata`) and the
  `OtaImageState` values (`New`, `PendingVerify`, `Valid`, `Invalid`,
  `Aborted`).
- The second-stage bootloader espflash 4.6 installs is a stock ESP-IDF v6.1
  build; its manifest (`espflash-4.6.0/resources/bootloaders/manifest.yaml`)
  sets only flash size for the C6. `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`
  is at its default, off: an image marked `PendingVerify` is NOT rolled back
  if it never confirms.
- Settings records (bus, mode, MQTT broker) live in the first sector of the
  `nvs` partition at 0x9000, found through the partition table
  (`fw/src/settings.rs`).
- The line ring (`modbus-sniffer-core`, `../stm32-modbus-sniffer/core`) has
  `RING_CONSUMERS = 2` (USB, TCP), each with its own wakeup signal.
- After every reset the bus task listens `SILENCE_CHECK` = 3 s for another
  master before it transmits (`fw/src/master.rs`). A reboot therefore leaves
  the bus without polls for ~3.5 s. ADR 0001 targets "never more than 2 s
  without a poll" and lists E09 tolerance as unmeasured.
- Port 4000 has no authentication; the LAN is trusted for commands. It is not
  trusted for replacing the firmware.

---

## Decision

1. A **custom partition table** with two OTA app slots, `nvs` kept at 0x9000.
2. A **rollback-enabled second-stage bootloader**, built from ESP-IDF v6.1 in
   a podman container with the espflash C6 config plus
   `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`, committed as a binary with its
   config and build script.
3. **OTA push over a dedicated TCP port (4002)**, one image per connection,
   **ed25519-signed**; the public key is compiled into the firmware. A host
   tool in this workspace builds, signs and sends the image.
4. **Self-confirmation**: a new image boots as `PendingVerify` and marks
   itself `Valid` only after it has proven it can do its job; otherwise the
   watchdog / an explicit reset hands control back to the bootloader, which
   rolls back.
5. **Short planned-reboot gap**: a reboot the firmware itself initiates (OTA)
   leaves a marker in RTC memory; the next boot skips the 3 s silence check
   once, because the only master on the bus a moment ago was us.
6. **A console port (4001)**: the same line protocol as 4000 (hello, lines,
   commands), with its own ring cursor that starts at the head (live tail, no
   replay). Port 4000 is unchanged and stays the capture daemon's.

The one USB flash that installs the partition table and bootloader also
installs the console port; after that every update goes over the network.

---

## Architecture Overview

### Component Breakdown

1. **Partition table** ([`fw/partitions.csv`](../../fw/partitions.csv))

   | Name | Type | SubType | Offset | Size |
   |---|---|---|---|---|
   | nvs | data | nvs | 0x9000 | 0x6000 |
   | otadata | data | ota | 0xf000 | 0x2000 |
   | phy_init | data | phy | 0x11000 | 0x1000 |
   | ota_0 | app | ota_0 | 0x20000 | 0x400000 |
   | ota_1 | app | ota_1 | 0x420000 | 0x400000 |

   4 MB per slot (5x today's image); the remaining ~7.8 MB stays unallocated.
   Referenced from `.cargo/config.toml`'s runner
   (`espflash flash --partition-table ... --bootloader ...`).

2. **Bootloader** ([`fw/bootloader/`](../../fw/bootloader/))
   - `sdkconfig.defaults`: the espflash manifest's C6 entry
     (`CONFIG_ESPTOOLPY_FLASHSIZE_64MB`, no compile-time date) plus
     `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`.
   - `build.sh`: runs `idf.py bootloader` in `docker.io/espressif/idf:release-v6.1`
     under podman, copies `bootloader.bin` out.
   - `esp32c6-bootloader-rollback.bin`: the committed result, so a flash does
     not need the container.
   - Only rollback is enabled. No secure boot, no flash encryption, no
     anti-rollback eFuse: all irreversible, none needed for a LAN device.

3. **OTA receiver** ([`fw/src/ota.rs`](../../fw/src/ota.rs))
   - Listens on TCP 4002, one connection at a time; a second connection
     while one runs is refused.
   - Wire format, little endian:

     ```
     header (128 bytes):
       magic       "WOTA"            4
       version     u8 = 1            1
       reserved    0                 3
       image_len   u32               4
       image_sha   SHA-256(image)    32
       target      "wfi028t-c6\0..." 16   refuse if not ours
       fw_version  "0.2.0\0..."      16   informational, logged
       signature   ed25519           64   over bytes 0..64 of this header
     image: image_len bytes (espflash save-image output)
     ```
   - Device answers single lines, the same `ok ...` / `err ...` style as the
     line interface: `ok header`, progress every 64 KB, `ok image`,
     `ok rebooting`, or one `err <reason>` and close.
   - Order of checks: magic/version/target -> signature over the header ->
     `image_len` fits the slot -> only then erase. Bytes are hashed while
     written; SHA mismatch or a short image leaves `otadata` untouched.
     After the SHA matches: check the ESP image header (magic 0xE9, chip id
     C6) -> `activate_next_partition`, state `New` -> close -> planned reboot
     (component 5).
   - The bus task keeps polling during the transfer. A settings write in
     flight finishes before the reboot; queued commands are failed with a
     new outcome `rebooting`.
   - Every step is a `# note` line, so the update is in the capture log.

4. **Self-confirmation** (in `fw/src/ota.rs`, called from `main.rs`)
   - At boot: if the running slot's state is `New`/`PendingVerify`, the image
     is on probation. `status` shows `ota=pending`, MQTT state carries it.
   - Confirmed (`set_current_ota_state(Valid)`) once, within 120 s of boot:
     the bus task is alive (watchdog fed), and
     - mode master: link up and 10 consecutive clean polls;
     - mode listen: 10 frames seen, or 30 s without a UART error if the bus is
       quiet;
     - and, if a broker is configured, MQTT connected once.
   - Not confirmed by 120 s: note it, then software reset. The rollback
     bootloader sees the slot still `PendingVerify` on the second boot, marks
     it `Aborted` and boots the previous slot. A crash or watchdog reset
     during probation ends the same way.
   - Probation does not block commands: HA keeps working on a good image. (See
     "Decisions you will probably want to tweak".)

5. **Planned reboot** ([`fw/src/main.rs`](../../fw/src/main.rs), bus task)
   - Before an OTA reboot: write `{magic, reason=ota, uptime}` with a CRC into
     RTC fast memory (`#[ram(rtc_fast, persistent)]`), then `software_reset`.
   - At boot: a valid marker AND reset reason "software reset" -> skip
     `SILENCE_CHECK` once and go straight to master if flash says master.
     Any other reset (power-on, brown-out, watchdog, panic) keeps the full
     3 s check. Marker cleared on read.
   - Target gap on the bus for an OTA reboot: under 1 s.

6. **Console port** ([`fw/src/net.rs`](../../fw/src/net.rs),
   `../stm32-modbus-sniffer/core`)
   - Core: `RING_CONSUMERS = 3`, new `CONSUMER_CONSOLE = 2`. The sniffer
     firmware ignores the third signal. Change lands in the sniffer repo
     (path dependency, ADR 0001).
   - One socket on TCP 4001. Newest connection wins, as on 4000.
   - On connect: hello line (same format, `replay=0`), cursor set to the
     ring head; lines from then on, `[DROPPED n lines]` markers if the
     client is slow. Commands are the full line interface (`cmd.rs`), replies
     go to the console only.
   - Port 4000 keeps its persistent cursor and replay; nothing on 4001 moves
     it, so the capture files stay complete.
   - `SOCKETS`: 4 -> 6 (console, OTA). RAM: console 512 + 4096 B buffers, OTA
     RX 4096 B + a 4 KB sector buffer.

7. **Host tool** ([`tools/fw-ota/`](../../tools/fw-ota/), workspace crate)
   - `fw-ota keygen`: writes `~/.config/wfi028t/ota-signing.key` (private,
     mode 0600, never in the repo; refuses to overwrite) and
     `fw/ota-signing.pub`.
   - `fw-ota push <host> [--elf path]`: `espflash save-image --chip esp32c6`
     -> header with SHA-256 and signature -> TCP 4002 -> prints the device's
     lines; exit status 0 only on `ok rebooting`. Optionally `--wait`: poll
     `sniffer-capture status --control ...` until `ota=valid` or the
     rollback shows up.
   - The public key reaches the firmware at build time:
     `include_bytes!` of `fw/ota-signing.pub` (committed; a public key).

### Data Flow / Interaction

```
fw-ota push host
  |  save-image, sha256, sign header
  v
TCP 4002 --> ota.rs: check header/signature --> erase ota_N --> stream+hash
                                                     |
                               sha ok, 0xE9/C6 ok    v
                     otadata: ota_N = New  --> RTC marker --> software reset
                                                     |
bootloader (rollback on) boots ota_N as PendingVerify
                                                     |
main.rs: probation; bus master without silence check (marker)
   confirmed in 120 s --> Valid
   not confirmed / crash --> reset --> bootloader: Aborted --> previous slot
```

---

## Alternatives Considered

### Stock espflash bootloader, rollback done by the app
- **The idea**: keep espflash's bootloader; the new image counts its own
  unconfirmed boots in RTC memory and switches `otadata` back itself.
- **Optimizes for**: no ESP-IDF container, nothing custom to flash.
- **Sharpest tradeoff**: an image that dies before its own boot counter runs
  (early panic, linker mistake, a hang before `main`) is never rolled back:
  USB reflash at the heat pump.
- **Bets on**: every bad image gets far enough into `main` to count.

### HTTP pull from a URL
- **The idea**: a command (`ota <url>`) makes the device download the image.
- **Optimizes for**: a standard flow; any web server can host images.
- **Sharpest tradeoff**: an HTTP client (and probably TLS for it to mean
  anything) in a no_std firmware, for a LAN of one device.
- **Bets on**: more than one device, or images hosted somewhere central.

### Image over MQTT
- **The idea**: publish the image in chunks to a command topic.
- **Optimizes for**: no new port; broker credentials as access control.
- **Sharpest tradeoff**: `MAX_PACKET` caps messages; chunking, ordering and
  resume over a pub/sub broker is a protocol of its own, and every client on
  the broker can publish to the topic.
- **Bets on**: the broker is the only thing that should talk to the device.

### Shared secret instead of signatures
- **The idea**: a token in the header compared to one in flash.
- **Optimizes for**: one line of code.
- **Sharpest tradeoff**: the secret travels in clear over the LAN on every
  update; anyone who sniffs one update can push anything.
- **Bets on**: nobody ever captures LAN traffic.

### Console as a second client on port 4000
- **The idea**: two clients on 4000, the first with the persistent cursor.
- **Optimizes for**: one port.
- **Sharpest tradeoff**: which client is "the logger" depends on connect
  order; a daemon restart while a console is open swaps roles and the log
  loses lines.
- **Bets on**: the daemon always connects first.

---

## Consequences

### Positive
- Updates without a USB cable, with automatic return to the last good image.
- Only images signed with the user's key are accepted.
- The capture log stays complete while someone works on the console.
- Every update is recorded in the capture log.

### Negative
- A binary bootloader to own, rebuilt from a container when ESP-IDF moves.
- One more USB flash, which also changes the flash layout.
- The signing key is a secret to keep; lose it and the next update is USB.
- Two more open ports on the LAN (4001 unauthenticated like 4000).

### Risks
- **Bus silence during the reboot -> E09.** Mitigation: planned-reboot skip
  of the silence check (target < 1 s); measure E09 tolerance with the mock
  first (`ctl_bench.py silence`) and then once on the real unit.
- **The new layout loses the settings.** `nvs` stays at 0x9000 and the
  records are found through the table, so they should survive; checked on
  the sniffer board first. If lost: `mode master` and `mqtt host` again.
- **espflash writes the app to `ota_0` but stale `otadata` selects
  something else.** Mitigation: the USB flash erases `otadata`
  (`--erase-parts otadata`) so the bootloader starts from `ota_0`.
- **Bricking the bootloader.** Only the one USB flash writes it; OTA never
  touches it. Tested on the sniffer board before the controller.

---

## What an Expert Would Ask

**Q: A malicious or broken image could pass the signature. What then?**
A: The signature only proves it came from your key. A broken signed image is
what probation is for; a malicious one requires the key. Downgrades are
allowed (any image signed with the key is accepted): that is how a bad
release is undone by hand, and an attacker without the key cannot use it.

**Q: The image confirms itself on 10 clean polls. Can a subtly wrong image
pass that and still do damage?**
A: Yes. Probation catches "does not run" (crash, no bus, no network), not
"runs wrong" (bad register math). That is the job of hp-model's host tests
and the mock bench (`tools/bench/hp_mock.py`); every image should go through
the mock before it reaches the heat pump. Not automated here.

**Q: What if power drops in the middle of an update?**
A: Before `activate_next_partition`, `otadata` still selects the old slot:
the half-written slot is never booted. After it, the new image is complete
and verified (SHA before activation), so the boot proceeds to probation.
`otadata` writes are the bootloader library's two-sector scheme.

**Q: Why trust a 3-line RTC marker to skip a safety check?**
A: The check exists to avoid fighting the stock controller. The marker is
only honoured on a software reset, which only the firmware itself causes,
seconds after it was the master. Power-on (stock controller possibly
plugged back in) always does the full check.

**Q: Flash erase stalls the CPU. Does the bus task miss frames?**
A: A 4 KB sector erase blocks flash access for ~30-50 ms. At 9600 baud the
128-byte UART FIFO holds ~130 ms, and the heat pump reply is 150 ms after
our request, so a sector at a time is safe; erasing a 64 KB block at once is
not, and is not done. To be confirmed on the bench (bad_crc / timeouts stay
0 during an update).

---

## Implementation Plan

### Decisions you will probably want to tweak

- **Probation does not block commands.**
  - Alternative: refuse `set` until `Valid` (outcome `pending-verify`).
  - Cost to change later: one check in `start_write` plus an outcome variant.
- **Confirmation criteria and the 120 s limit.**
  - Alternative: confirm on link-up alone, or require MQTT always.
  - Cost to change later: one function; no format impact.
- **Ports 4001 (console) and 4002 (OTA).**
  - Alternative: OTA as a binary mode on the console port.
  - Cost to change later: host tool and firmware together; trivial before
    the first OTA image exists, annoying after (old images speak the old
    port).
- **Header format (128 bytes, signature over the header only).**
  - Alternative: sign the whole image streamed (ed25519ph).
  - Cost to change later: every deployed image must understand the next
    one's header, so the version byte has to be honoured from v1 on. Most
    expensive thing here to get wrong; review it.
- **4 MB slots.**
  - Cost to change later: another USB flash. Generous on purpose.

### Known unknowns and how the plan absorbs them

- **E09 tolerance**: default target < 1 s gap on OTA reboot. Pivot: if the
  mock bench or the real unit shows E09 below ~2 s, OTA reboots wait for a
  status poll to have just completed and skip ESP-IDF-style boot delays.
- **espflash `--erase-parts otadata` with a custom table**: default assumes
  it works. Pivot: `espflash erase-region 0xf000 0x2000` before the flash.
- **Settings surviving the table change**: default assumes they do. Pivot:
  re-enter them (three commands) and note it in the README.
- **Bootloader build in the container**: default `idf.py bootloader` in
  `espressif/idf:release-v6.1`. Pivot: espflash's own
  `cargo xtask build-bootloaders` with an edited manifest.

### The mechanical work

- `fw/partitions.csv`, `fw/bootloader/` (config, script, binary), runner
  flags in `fw/.cargo/config.toml`.
- `fw/src/ota.rs`: receiver, probation, planned-reboot marker; host tests for
  header parsing and signature checks in a pure module (as `mqtt/entity.rs`).
- Crates: `ed25519-compact` or `salty` (no_std verify), `sha2` (or the C6 SHA
  peripheral); host tool: `ed25519-dalek`, `sha2`. Added with `cargo add`.
- `net.rs`: console socket and task, `SOCKETS = 6`; `cmd.rs` replies routed
  per client; `status` gains `ota=` and `console=`.
- Sniffer core: `RING_CONSUMERS = 3`, `CONSUMER_CONSOLE`; sniffer firmware
  builds unchanged.
- Bench: OTA to the sniffer board (controller fw, listen mode), then with the
  mock in master mode: push, bad signature, wrong target, truncated image,
  power cut mid-transfer, an image that never confirms (feature flag), E09
  gap measurement.
- Docs: README (update procedure, key handling), register-map/ADR 0001 cross
  reference, `status` field list.

Review asks (settled 2026-10-04):
1. Probation lets HA commands through: yes.
2. Header format as above, signature over the header only: yes.
3. Separate ports 4001 (console) and 4002 (OTA): yes.
4. Private key at `~/.config/wfi028t/ota-signing.key`, outside the repo: yes;
   the README says where it lives and what to do if it is lost.

---

## Open Questions

**Architecture-changers**
- [ ] How long does the heat pump tolerate a silent bus before E09, and does
  E09 latch? (Decides whether the < 1 s target is enough.)

**Behavior definers**
- [ ] On a rollback, should the device raise anything in HA beyond the
  `ota=` field (e.g. a persistent "update failed" binary sensor)?
- [ ] Should `fw-ota push` refuse to update while the heat pump is mid-start
  (compressor ramping), or is that the operator's call?
