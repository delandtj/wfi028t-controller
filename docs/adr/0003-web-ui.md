# A small built-in web page: controls, state and signed image upload

**Status**: Proposed
**Date**: 2026-10-04

---

## Context

The controller is driven from three places today:

- Home Assistant over MQTT ([`fw/src/mqtt.rs`](../../fw/src/mqtt.rs)), the
  everyday interface.
- The line interface on USB, TCP 4000 and the console port 4001
  ([`fw/src/cmd.rs`](../../fw/src/cmd.rs)), for the bench and for diagnosis.
- `fw-ota push` to port 4002 for updates (ADR 0002), from the one PC that has
  the toolchain and the signing key.

What is missing is a way to do the basics from a phone or any browser when
Home Assistant or the broker is down, or when the person at hand is not at
that PC: switch the unit on or off, change a setpoint, see whether the bus link
is up and what the water temperatures are, and install an image someone has
already signed.

Constraints:

- **The bus comes first.** One executor runs everything except the radio's own
  threads. Nothing the web server does may hold it long enough to stretch a
  poll slot (ADR 0002 measured what 0.8 s of blocking does to the bus).
- **One source of truth for what is writable.** The ranges and options live in
  the entity table ([`fw/src/mqtt/entity.rs`](../../fw/src/mqtt/entity.rs)) and
  in `hp_model`; the page must not carry a second copy that can drift.
- **No new trust.** The LAN is trusted for commands (ADR 0002: ports 4000 and
  4001 are unauthenticated). Firmware is not: only an image signed with the
  OTA key is ever written. A browser must not weaken either line.
- **RAM**: 96 KiB heap, about 120 KiB left for the stack, six sockets in the
  embassy-net stack. One more socket with small buffers fits; a general-purpose
  web framework with its own allocator appetite is not wanted.

A browser is a different kind of client from `nc`: any web page open on a LAN
machine can make it send requests to the controller. That, not the page
itself, is what this ADR is mostly about.

---

## Decision

A hand-rolled HTTP/1.1 server on port 80, one connection at a time, serving
one embedded page and four endpoints:

| Method, path | Does |
|---|---|
| `GET /` | the page (static, embedded in the image) |
| `GET /api/entities` | the entity table: id, name, kind, min/max/options, writable |
| `GET /api/state` | the MQTT state document plus `link`, `mode`, `ota`, uptime, version |
| `POST /api/set/<id>` | body = the same payload MQTT takes on `wfi028t/<id>/set` |
| `POST /api/ota` | body = a signed image file; same checks as port 4002 |

Commands go through `entity::parse_set` and the `master::COMMANDS` queue,
exactly like a Home Assistant command, so the web page cannot do anything HA
cannot. Operating mode (listen/master), bus settings and MQTT configuration
stay console-only.

Every request must carry a `Host` that names this device (its IPv4 literal or
the `wfi-controller` hostname) and every `POST` must carry `X-WFI: 1`. The
custom header makes any cross-origin `fetch` a preflighted request; the server
never answers `OPTIONS` with permission, so a browser on another origin cannot
send it. The `Host` check defeats DNS rebinding. Forms and plain links cannot
set headers, so they are refused.

The upload takes a file that `fw-ota sign` produced on the PC: the 140-byte
header (ADR 0002) followed by the image. The device verifies it exactly as it
does on port 4002 - signature before anything else, then the SHA-256 over the
written image, then probation and rollback.

---

## Architecture Overview

### Component Breakdown

1. **HTTP server** ([`fw/src/web.rs`](../../fw/src/web.rs), new)
   - One task, one `TcpSocket` on port 80, accept - serve - close. A second
     client waits in the backlog; there is no keep-alive.
   - Request parsing: request line and headers into a 1 KiB buffer, `Content-
     Length` required for a body, no chunked encoding, no multipart. Anything
     else is `400` or `411` and the connection closes.
   - Guard, before routing: `Host` allowlist, `X-WFI: 1` on `POST`, `GET`/`POST`
     only. `OPTIONS` gets `405` with no `Access-Control-*` headers.
   - Header timeout 5 s, body inactivity timeout 30 s (the OTA chunk timeout).
   - Responses: `Connection: close`, `Cache-Control: no-store`, JSON bodies
     written with `core::fmt::Write` into a stack buffer, the page streamed
     from flash in socket-sized writes.
   - Socket buffers: RX 4 KiB (one flash sector per read during an upload),
     TX 2 KiB. `SOCKETS` in [`fw/src/net.rs`](../../fw/src/net.rs) goes from 6
     to 7.

2. **OTA install core** ([`fw/src/ota.rs`](../../fw/src/ota.rs), refactor)
   - Today `receive(&mut TcpSocket)` reads, verifies, writes and answers in
     `ok`/`err` lines as it goes. It splits into a transport-free
     `install(source, progress) -> Result<(), Reject>` and two front ends:
     the port 4002 one (lines as now, byte-for-byte the same protocol) and the
     HTTP one (no progress lines; one JSON verdict at the end).
   - One install at a time across both ports: an `OTA_BUSY` flag taken before
     the header is read. The loser gets `err busy` / `409`.
   - The reboot stays where it is: after the verdict is flushed, `planned_reset`
     with the marker, so the bus-gap work in ADR 0002 applies unchanged.

3. **Command endpoint** (in `web.rs`)
   - `POST /api/set/<id>`: `<id>` must be a writable entity; the body is the MQTT
     payload (`ON`/`OFF`, `heat`, `35`). `entity::parse_set` builds the
     `hp_model::Command`; the request then waits for the outcome on
     `master::OUTCOMES`, like `set` on the console (`OUTCOME_WAIT`, 6 s), and
     answers `{"outcome": "...", "detail": "..."}` with `200`, or `422` for a
     value `parse_set` rejects, `503` when not in master mode.

4. **State and entity endpoints** (in `web.rs`)
   - `/api/state`: `mqtt::json::write_state` (the document HA already gets)
     wrapped with `link`, `mode`, `ota`, `uptime_ms`, `fw`. Works without a
     broker: the view is built from `master::SNAPSHOT`, not from MQTT.
   - `/api/entities`: generated from `entity::ENTITIES` at request time. The page
     builds its switches, selects and number inputs from it, so a new entity
     or a changed range needs no page change.

5. **The page** ([`fw/web/index.html`](../../fw/web/index.html), new;
   `include_bytes!`)
   - One file, inline CSS and JS, no external resources (the device may be the
     only thing reachable). Target under 12 KiB.
   - Top: link, mode, OTA state, inlet/outlet/ambient, compressor Hz, refreshed
     by polling `/api/state` every 2 s while the tab is visible.
   - Controls: one row per writable entity. A change sends one `POST` and shows
     the outcome next to the control; nothing is optimistic, the control
     re-reads from the next state.
   - Update: file picker, an upload progress bar from `XMLHttpRequest`'s
     upload events, the verdict, then polling `/api/state` through the reboot
     until `ota=valid` or `ota=aborted` - the same thing `fw-ota push --wait`
     reports.

6. **`fw-ota sign`** ([`tools/fw-ota/src/main.rs`](../../tools/fw-ota/src/main.rs))
   - `fw-ota sign [--elf <path>] [--out <file>]` writes header + image to
     `wfi-controller-<version>.wota`, the same bytes `push` sends. `push` gains
     `--file <wota>` to send a signed file as-is.

7. **`status` and docs**
   - `status` gains `web=idle|connected`, after `console=`.
   - README: the page, the guard, the `.wota` workflow. The HA doc gets one
     line pointing at the page as the fallback.

### Data Flow / Interaction

```
browser --GET /--------------> web task --> embedded page
        --GET /api/state----->          --> SNAPSHOT + write_state
        --POST /api/set/p01-->  guard   --> parse_set --> COMMANDS --> bus task
                              <--------------- OUTCOMES <--------------+
        --POST /api/ota------>  guard   --> OTA_BUSY --> install() --> flash
                              <-- verdict -- planned_reset (marker) --> bootloader

fw-ota push --4002----------------------> OTA_BUSY --> install()   (unchanged)
```

---

## Alternatives Considered

### picoserve (or another embedded HTTP framework)
- **The idea**: use an existing `no_std` async HTTP server with routing.
- **Optimizes for**: less parsing code to own.
- **Sharpest tradeoff**: a dependency tied to specific embassy-net and
  embassy-time versions in a firmware that already tracks fast-moving esp-hal
  crates; generic-heavy routing for five routes.
- **Bets on**: the page staying at five routes. If it grows a real API, this
  revives.

### Raw command lines over HTTP (`POST /api/cmd` with `set p01 35`)
- **The idea**: forward the body to `cmd::execute`, like the console.
- **Optimizes for**: zero new command code; everything the console can do.
- **Sharpest tradeoff**: it also exposes `mode`, `bus` and `mqtt user` to a
  browser, a much larger surface for a cross-site mistake.
- **Bets on**: the CSRF guard never having a hole. The entity path bets less.

### A token instead of (or on top of) the header guard
- **The idea**: a shared secret in a cookie or header, set on the console.
- **Optimizes for**: protection against other LAN hosts, not only other web
  origins.
- **Sharpest tradeoff**: a secret to provision, store, and lose, for a LAN
  that ports 4000/4001 already trust without one.
- **Bets on**: the LAN staying trusted. If the controller ever sits on a
  guest or shared network, this revives - for all three ports, not just 80.

### Unsigned upload, signed on the device
- **The idea**: upload a plain image; the device needs no key.
- **Optimizes for**: convenience: any build, no PC step.
- **Sharpest tradeoff**: anyone on the LAN can install anything. Throws away
  the one guarantee ADR 0002 exists for.
- **Bets on**: nothing that holds. Rejected outright.

### Server-sent events for the state
- **The idea**: push state changes to the page instead of polling.
- **Optimizes for**: instant updates.
- **Sharpest tradeoff**: holds the only HTTP socket for as long as a tab is
  open, which blocks commands and uploads from every other client.
- **Bets on**: one client at a time. A 2 s poll is honest enough for a heat
  pump.

---

## Consequences

### Positive
- The basics work from any browser without Home Assistant or the broker.
- An update needs only a signed file and a browser; the key never leaves the
  PC.
- Commands keep one validation path (entity table -> `parse_set` -> `hp_model`)
  for HA and the page alike.

### Negative
- Port 80 is a new, browser-reachable surface; the guard is now part of the
  security model and has to stay correct.
- One more socket and one more task on the shared executor.
- The OTA receiver is refactored after it was tested; the failure cases in
  `tools/bench/ota_fail.py` have to pass again on both ports.

### Risks
- **A hole in the guard** (a header we forgot a browser can send cross-origin,
  a `Host` form we accept by mistake). Mitigation: allowlist, not denylist;
  bench tests that send the cross-origin shapes (form post, `text/plain`
  fetch, missing `X-WFI`, foreign `Host`) and expect refusal.
- **A slow or stalled client holding the socket.** Mitigation: header and body
  timeouts; one connection at a time means the worst case is a 30 s wait for
  the next client, never a stuck bus.
- **Upload over flaky WiFi from a phone.** The image is not activated unless
  every byte checks out; a failed upload leaves the running image untouched
  (bench-tested on 4002, same core).

---

## What an Expert Would Ask

**Q: Can a page on another site switch the heat pump off?**
A: Not through a browser's normal rules. `POST` needs `X-WFI: 1`; a
cross-origin request with a custom header is preflighted, and the server never
grants the preflight. Forms, image tags and links cannot set headers. DNS
rebinding (a hostile name resolving to the controller) is stopped by the
`Host` allowlist. What remains is anything that is not a browser on the LAN -
which can already do the same on port 4000, by design.

**Q: The upload is ~870 KB over a single slow socket. What does it do to the
bus?**
A: The same as a push to 4002 does today: the receive loop awaits the socket
and writes one sector at a time, and the bench showed no poll timeouts during
pushes. The HTTP front end adds header parsing before the body and nothing
inside the loop.

**Q: Two browsers, or a browser and `fw-ota push`, update at once.**
A: `OTA_BUSY` is taken before the header is read, across both ports; the
second gets `409` / `err busy` and nothing of it is written.

**Q: What does the page show if the bus link is down or the controller is in
listen mode?**
A: State still loads (stale values are marked by `link=down` and the
snapshot age). Controls are disabled with the reason; a `POST` anyway returns
`503`, from the same check the console uses.

**Q: Why not HTTPS?**
A: No certificate story on a LAN device that would not train people to click
through warnings, and TLS on the C6 would cost RAM the bus task's neighbours
need. Integrity of firmware comes from the signature, not the transport;
commands are LAN-trusted like the other ports.

---

## Implementation Plan

### Decisions you will probably want to tweak

- **Commands through the entity table only.**
  - Choice: `POST /api/set/<id>`, writable entities only; mode/bus/MQTT stay
    console-only.
  - Alternative: forward raw command lines to `cmd::execute`.
  - Cost to change later: small - one more route - but it widens the guard's
    blast radius.
- **Guard = `Host` allowlist + `X-WFI: 1`, no token.**
  - Choice: as above.
  - Alternative: a token set with a console command.
  - Cost to change later: moderate; the page needs a login step and the token
    needs storage.
- **Port 80, plain HTTP.**
  - Choice: 80.
  - Alternative: 8080, to leave 80 free.
  - Cost to change later: trivial in code; bookmarks break.
- **Page built from `/api/entities`.**
  - Choice: generated controls.
  - Alternative: a hand-written page with fixed controls.
  - Cost to change later: low; the endpoint can stay either way.

### Known unknowns and how the plan absorbs them

- **Phone browsers and `Host`**: default accepts the IPv4 literal and
  `wfi-controller` with or without a domain suffix. Pivot if mDNS or the
  router hands out a different name: make the hostname list a setting.
- **Upload throughput through an HTTP socket**: default 4 KiB RX buffer.
  Pivot if an upload takes much longer than a `fw-ota push` (~30 s): same
  buffer size as `OTA_RX_BUF`, or share that buffer under `OTA_BUSY`.
- **Executor stalls while serving the page**: default plain socket writes from
  flash. Pivot if the mock bench shows a stretched slot while a page loads:
  smaller writes with a yield between them.

### The mechanical work

- `fw/src/web.rs`: server task, parser, guard, routes; host tests for the
  parser and the guard (the same `#[path]` trick `fw-ota` uses for the header).
- `fw/src/ota.rs`: `install()` split, `OTA_BUSY`, two front ends.
- `fw/src/net.rs`: `SOCKETS = 7`, spawn the web task.
- `fw/web/index.html`.
- `tools/fw-ota`: `sign`, `push --file`.
- `tools/bench/ota_fail.py`: run every case against both ports; a
  `web_guard.py` bench script for the cross-origin shapes.
- README and HA doc lines; `status` gains `web=`.

Review asks:

1. Commands limited to the writable entities (no `mode master`/`listen` from
   the page) - yes/no?
2. No token: `Host` allowlist plus `X-WFI` header only - yes/no?
3. Port 80 or 8080?
4. Should the page be able to switch listen/master at all, behind a confirm
   step - yes/no?

---

## Open Questions

**Architecture-changers**
- [ ] Is a token needed after all (is the LAN really trusted for a browser-
      reachable port)? Changes the page, the guard and storage.

**Behavior definers**
- [ ] Does the page offer listen/master switching (review ask 4)?
- [ ] Should a `.wota` from an older firmware version be refused (downgrade
      protection), or is "signed" enough as in ADR 0002?
