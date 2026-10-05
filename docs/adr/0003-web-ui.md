# A small built-in web page: controls, state, settings and signed image upload

**Status**: Proposed
**Date**: 2026-10-04
**Updated**: 2026-10-04 (the page becomes the product's front door, ADR 0004:
an admin password and a login session replace "the LAN is trusted"; the
server also carries the settings and setup pages; port 80 settled)

---

## Context

The controller is driven from three places today:

- Home Assistant over MQTT ([`fw/src/mqtt.rs`](../../fw/src/mqtt.rs)), the
  everyday interface.
- The line interface on USB, TCP 4000 and the console port 4001
  ([`fw/src/cmd.rs`](../../fw/src/cmd.rs)), for the bench and for diagnosis.
- `fw-ota push` to port 4002 for updates (ADR 0002), from the one PC that has
  the toolchain and the signing key.

The repository is meant to become something anyone with the board can use:
build or download, flash, boot, open a web page, configure, run (ADR 0004).
That makes the web page the front door, not a fallback. It has to:

- do the basics from any browser: switch the unit on or off, change a
  setpoint, see the link and the water temperatures - also when Home Assistant
  or the broker is down;
- install an image someone has already signed;
- carry the settings that today need a console or a rebuild: WiFi, MQTT, the
  bus, master/listen (ADR 0004 defines those pages; this ADR defines the
  server, the guard and the login they sit behind).

Constraints:

- **The bus comes first.** One executor runs everything except the radio's own
  threads. Nothing the web server does may hold it long enough to stretch a
  poll slot (ADR 0002 measured what 0.8 s of blocking does to the bus).
- **One source of truth for what is writable.** The ranges and options live in
  the entity table ([`fw/src/mqtt/entity.rs`](../../fw/src/mqtt/entity.rs)) and
  in `hp_model`; the page must not carry a second copy that can drift.
- **Firmware trust does not move.** Only an image signed with the OTA key is
  ever written (ADR 0002). A browser must not weaken that.
- **Command trust does move.** Ports 4000/4001 trust the LAN, which was fine
  for a bench tool used by its author. A page that changes WiFi and MQTT
  credentials and can take over the bus, on whatever network a user puts it,
  needs a password.
- **RAM**: 96 KiB heap, about 120 KiB left for the stack, six sockets in the
  embassy-net stack. One or two more sockets with small buffers fit; a
  general-purpose web framework with its own allocator appetite is not wanted.

A browser is a different kind of client from `nc`: any web page open on a LAN
machine can make it send requests to the controller. Keeping other origins
out is the second half of this ADR, next to the login.

---

## Decision

A hand-rolled HTTP/1.1 server on port 80, one connection at a time, serving
embedded pages and a small JSON API:

| Method, path | Login | Does |
|---|---|---|
| `GET /` | no | the page (static, embedded in the image) |
| `GET /api/state` | no | the MQTT state document plus `link`, `mode`, `ota`, uptime, version |
| `POST /api/login` | - | password -> session cookie |
| `POST /api/logout` | yes | ends the session |
| `GET /api/entities` | no | the entity table: id, name, kind, min/max/options, writable |
| `POST /api/set/<id>` | yes | body = the same payload MQTT takes on `wfi028t/<id>/set` |
| `POST /api/ota` | yes | body = a signed image file; same checks as port 4002 |
| `GET/POST /api/settings/...` | yes | WiFi, MQTT, bus, mode, password - specified in ADR 0004 |

Read-only state needs no login, so a glance at the temperatures from a phone
stays one tap. Everything that changes something needs one.

**Login.** One admin password, set during setup (ADR 0004). The device stores
a salted, iterated hash (PBKDF2-HMAC-SHA256), never the password. A correct
`POST /api/login` returns a random 128-bit session token in a cookie
(`HttpOnly; SameSite=Strict; Path=/`). Sessions live in RAM, a handful at most,
and expire after 12 h idle or at reboot. Five wrong passwords in a row lock
login for a minute. A forgotten password is a factory reset (ADR 0004).

**Other origins.** On top of the login, unchanged from the first draft:

- every request must carry a `Host` that names this device (its IPv4 literal
  or the `wfi-controller` hostname, with or without a domain suffix) - this
  defeats DNS rebinding;
- every `POST` must carry `X-WFI: 1` - a cross-origin `fetch` with a custom
  header is preflighted, and the server never grants `OPTIONS`, so forms,
  links and scripts on other sites cannot send it.

`SameSite=Strict` alone would keep the cookie off cross-site requests in
current browsers; the header and the `Host` check keep that true for old
browsers and for setup mode, where there is no session yet.

Commands go through `entity::parse_set` and the `master::COMMANDS` queue,
exactly like a Home Assistant command, so the control part of the page cannot
do anything HA cannot. Master/listen, bus settings, WiFi and MQTT are settings
pages behind the same login (ADR 0004), not entity commands.

The upload takes a file that `fw-ota sign` produced: the 140-byte header
(ADR 0002) followed by the image. The device verifies it exactly as it does on
port 4002 - signature before anything else, then the SHA-256 over the written
image, then probation and rollback.

---

## Architecture Overview

### Component Breakdown

1. **HTTP server** ([`fw/src/web.rs`](../../fw/src/web.rs), new)
   - One task, one `TcpSocket` on port 80, accept - serve - close. A second
     client waits in the backlog; there is no keep-alive.
   - The same task serves the station interface and, in setup mode, the
     access-point interface (ADR 0004): one listener per active interface.
   - Request parsing: request line and headers into a 1 KiB buffer, `Content-
     Length` required for a body, no chunked encoding, no multipart. Anything
     else is `400` or `411` and the connection closes.
   - Guard, before routing: `Host` allowlist, `X-WFI: 1` on `POST`, `GET`/`POST`
     only. `OPTIONS` gets `405` with no `Access-Control-*` headers. Then the
     session check for routes that need a login (`401`).
   - Header timeout 5 s, body inactivity timeout 30 s (the OTA chunk timeout).
   - Responses: `Connection: close`, `Cache-Control: no-store`, JSON bodies
     written with `core::fmt::Write` into a stack buffer, the page streamed
     from flash in socket-sized writes.
   - Socket buffers: RX 4 KiB (one flash sector per read during an upload),
     TX 2 KiB. `SOCKETS` in [`fw/src/net.rs`](../../fw/src/net.rs) goes from 6
     to 7.

2. **Login** ([`fw/src/web/auth.rs`](../../fw/src/web/auth.rs), new)
   - Password record in the settings sector (ADR 0004 owns the layout): salt
     16 B, iteration count, hash 32 B, CRC. No record = no password set = the
     device is unconfigured.
   - PBKDF2-HMAC-SHA256, 10 000 iterations (`sha2` is already a dependency;
     `hmac` and `pbkdf2` are small, `no_std`). Hashing runs once per login, in
     slices with a yield in between, so a login never stretches a poll slot.
   - Session table: 4 entries of (token, last use), in RAM. Tokens from the
     hardware RNG (the radio is up by then, so it is a true RNG).
   - Lockout: 5 failures -> 60 s of `429` for every login attempt.

3. **OTA install core** ([`fw/src/ota.rs`](../../fw/src/ota.rs), refactor)
   - Today `receive(&mut TcpSocket)` reads, verifies, writes and answers in
     `ok`/`err` lines as it goes. It splits into a transport-free
     `install(source, progress) -> Result<(), Reject>` and two front ends:
     the port 4002 one (lines as now, byte-for-byte the same protocol) and the
     HTTP one (no progress lines; one JSON verdict at the end).
   - One install at a time across both ports: an `OTA_BUSY` flag taken before
     the header is read. The loser gets `err busy` / `409`.
   - The reboot stays where it is: after the verdict is flushed, `planned_reset`
     with the marker, so the bus-gap work in ADR 0002 applies unchanged.

4. **Command endpoint** (in `web.rs`)
   - `POST /api/set/<id>`: `<id>` must be a writable entity; the body is the MQTT
     payload (`ON`/`OFF`, `heat`, `35`). `entity::parse_set` builds the
     `hp_model::Command`; the request then waits for the outcome on
     `master::OUTCOMES`, like `set` on the console (`OUTCOME_WAIT`, 6 s), and
     answers `{"outcome": "...", "detail": "..."}` with `200`, or `422` for a
     value `parse_set` rejects, `503` when not in master mode.

5. **State and entity endpoints** (in `web.rs`)
   - `/api/state`: `mqtt::json::write_state` (the document HA already gets)
     wrapped with `link`, `mode`, `ota`, `uptime_ms`, `fw`. Works without a
     broker: the view is built from `master::SNAPSHOT`, not from MQTT.
   - `/api/entities`: generated from `entity::ENTITIES` at request time. The page
     builds its switches, selects and number inputs from it, so a new entity
     or a changed range needs no page change.

6. **The pages** ([`fw/web/`](../../fw/web), new; `include_bytes!`)
   - Plain HTML files, inline CSS and JS, no external resources (in setup mode
     the device is the only thing reachable). Target under 12 KiB each.
   - `index.html`: link, mode, OTA state, inlet/outlet/ambient, compressor Hz,
     refreshed by polling `/api/state` every 2 s while the tab is visible; the
     controls (one row per writable entity, shown after login); the update
     card (file picker, upload progress from `XMLHttpRequest`'s upload events,
     the verdict, then polling through the reboot until `ota=valid` or
     `ota=aborted`).
   - Nothing is optimistic: a control shows the outcome of its `POST` and
     re-reads from the next state.
   - The setup and settings pages are ADR 0004's.

7. **`fw-ota sign`** ([`tools/fw-ota/src/main.rs`](../../tools/fw-ota/src/main.rs))
   - `fw-ota sign [--elf <path>] [--out <file>]` writes header + image to
     `wfi-controller-<version>.wota`, the same bytes `push` sends. `push` gains
     `--file <wota>` to send a signed file as-is.

8. **`status` and docs**
   - `status` gains `web=idle|connected` after `console=`, and
     `sessions=<n>`.
   - README: the page, the login, the guard, the `.wota` workflow. The HA doc
     gets one line pointing at the page.

### Data Flow / Interaction

```
browser --GET /--------------> web task --> embedded page
        --GET /api/state----->  guard   --> SNAPSHOT + write_state
        --POST /api/login---->  guard   --> PBKDF2 check --> session cookie
        --POST /api/set/p01-->  guard + session --> parse_set --> COMMANDS --> bus task
                              <--------------------------- OUTCOMES <--------------+
        --POST /api/ota------>  guard + session --> OTA_BUSY --> install() --> flash
                              <-- verdict -- planned_reset (marker) --> bootloader

fw-ota push --4002--------------------------------> OTA_BUSY --> install()   (unchanged)
```

---

## Alternatives Considered

### No login: the LAN is trusted (the first draft)
- **The idea**: keep ports 4000/4001's trust model; only keep other web
  origins out.
- **Optimizes for**: zero setup, nothing to forget.
- **Sharpest tradeoff**: anyone on the network can change the WiFi and MQTT
  credentials and take over the bus, from a browser.
- **Bets on**: the device only ever living on its author's own network. True
  for the bench; not for a repository others flash. Revives for a build
  option "no login" if that ever matters.

### HTTP Basic auth instead of a session
- **The idea**: the browser sends the password with every request.
- **Optimizes for**: no session table, no login form.
- **Sharpest tradeoff**: a PBKDF2 check on every request (or a weak hash), the
  browser's ugly prompt, and no way to log out.
- **Bets on**: requests being rare. The 2 s state poll is not.

### picoserve (or another embedded HTTP framework)
- **The idea**: use an existing `no_std` async HTTP server with routing.
- **Optimizes for**: less parsing code to own.
- **Sharpest tradeoff**: a dependency tied to specific embassy-net and
  embassy-time versions in a firmware that already tracks fast-moving esp-hal
  crates; generic-heavy routing for a dozen routes.
- **Bets on**: the API staying small. If it grows much, this revives.

### Raw command lines over HTTP (`POST /api/cmd` with `set p01 35`)
- **The idea**: forward the body to `cmd::execute`, like the console.
- **Optimizes for**: zero new command code; everything the console can do.
- **Sharpest tradeoff**: the console's grammar becomes the web API, with every
  command at one permission level.
- **Bets on**: the console grammar being a good API. It is a good bench tool.

### Unsigned upload, signed on the device
- **The idea**: upload a plain image; the device needs no key.
- **Optimizes for**: convenience: any build, no PC step.
- **Sharpest tradeoff**: anyone who has the password can install anything,
  and the password travels over plain HTTP. Throws away the one guarantee
  ADR 0002 exists for.
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
- Settings that needed a console or a rebuild get a page (ADR 0004).

### Negative
- Port 80 is a new, browser-reachable surface; the guard and the login are
  now part of the security model and have to stay correct.
- One more socket and one more task on the shared executor.
- A password to set, and to lose (factory reset).
- The OTA receiver is refactored after it was tested; the failure cases in
  `tools/bench/ota_fail.py` have to pass again on both ports.
- Ports 4000/4001 stay unauthenticated, so the login protects the page, not
  the device: anyone on the LAN with `nc` can still send `set`, `mode` and
  `mqtt`. See the expert questions.

### Risks
- **A hole in the guard** (a header we forgot a browser can send cross-origin,
  a `Host` form we accept by mistake). Mitigation: allowlist, not denylist;
  bench tests that send the cross-origin shapes (form post, `text/plain`
  fetch, missing `X-WFI`, foreign `Host`, no cookie) and expect refusal.
- **Password over plain HTTP.** Anyone sniffing the LAN can read it at login.
  Accepted (see the HTTPS question); the WiFi's own encryption covers the air.
- **A slow or stalled client holding the socket.** Mitigation: header and body
  timeouts; one connection at a time means the worst case is a 30 s wait for
  the next client, never a stuck bus.
- **Upload over flaky WiFi from a phone.** The image is not activated unless
  every byte checks out; a failed upload leaves the running image untouched
  (bench-tested on 4002, same core).

---

## What an Expert Would Ask

**Q: A login on port 80 while 4000/4001 accept anything from the LAN - what is
the point?**
A: Fair. The login keeps browsers honest and keeps casual users from changing
things; it does not stop someone on the LAN who reads the README. ADR 0004
takes the next step: the line ports listen on the station interface only, and
a setting (default on for released images, off for bench builds) restricts
4000/4001 to read-only (`status`, the stream) unless enabled from the logged-in
page. Until then the honest statement is "the page is protected, the device
trusts its LAN".

**Q: Can a page on another site switch the heat pump off?**
A: Not through a browser's normal rules. `POST` needs `X-WFI: 1` (forces a
preflight the server never grants) and a session cookie that is
`SameSite=Strict`. Forms, image tags and links cannot set headers. DNS
rebinding is stopped by the `Host` allowlist.

**Q: The upload is ~870 KB over a single slow socket. What does it do to the
bus?**
A: The same as a push to 4002 does today: the receive loop awaits the socket
and writes one sector at a time, and the bench showed no poll timeouts during
pushes. The HTTP front end adds header parsing before the body and nothing
inside the loop.

**Q: 10 000 PBKDF2 iterations on a 160 MHz RISC-V, on the executor the bus
runs on?**
A: Roughly 20 000 SHA-256 blocks, a few hundred milliseconds in total. Run in
slices of a few hundred iterations with a yield between them, the bus task
gets the CPU at every slot boundary. It happens once per login, not per
request.

**Q: Why not HTTPS?**
A: No certificate story on a LAN device that would not train people to click
through warnings, and TLS on the C6 would cost RAM the bus task's neighbours
need. Firmware integrity comes from the signature, not the transport. The
password crossing the LAN in clear at login is the accepted cost.

---

## Implementation Plan

### Decisions you will probably want to tweak

- **Read-only state without login.**
  - Choice: `GET /` and `GET /api/state` public.
  - Alternative: everything behind the login.
  - Cost to change later: one flag per route.
- **Commands through the entity table only.**
  - Choice: `POST /api/set/<id>`, writable entities only; mode/bus/WiFi/MQTT
    are settings (ADR 0004).
  - Alternative: forward raw command lines to `cmd::execute`.
  - Cost to change later: small - one more route.
- **Session cookie with PBKDF2 at login.**
  - Choice: as above, 12 h idle expiry, 4 sessions, lost at reboot.
  - Alternative: Basic auth.
  - Cost to change later: moderate; the page's login flow changes.
- **Page built from `/api/entities`.**
  - Choice: generated controls.
  - Alternative: a hand-written page with fixed controls.
  - Cost to change later: low; the endpoint can stay either way.

### Known unknowns and how the plan absorbs them

- **Phone browsers and `Host`**: default accepts the IPv4 literal and
  `wfi-controller` with or without a domain suffix. Pivot if mDNS or the
  router hands out a different name: derive the list from the configured
  hostname (ADR 0004).
- **Upload throughput through an HTTP socket**: default 4 KiB RX buffer.
  Pivot if an upload takes much longer than a `fw-ota push` (~30 s): same
  buffer size as `OTA_RX_BUF`, or share that buffer under `OTA_BUSY`.
- **Executor stalls while serving pages or hashing**: default plain socket
  writes from flash, PBKDF2 in slices. Pivot if the mock bench shows a
  stretched slot during a page load or a login: smaller writes, smaller
  slices.

### The mechanical work

- `fw/src/web.rs`, `fw/src/web/auth.rs`: server task, parser, guard, sessions,
  routes; host tests for the parser, the guard and the session table (the same
  `#[path]` trick `fw-ota` uses for the header).
- `fw/src/ota.rs`: `install()` split, `OTA_BUSY`, two front ends.
- `fw/src/net.rs`: `SOCKETS = 7`, spawn the web task.
- `fw/web/index.html`.
- `tools/fw-ota`: `sign`, `push --file`.
- `tools/bench/ota_fail.py`: run every case against both ports; a
  `web_guard.py` bench script for the cross-origin shapes and the login.
- README and HA doc lines; `status` gains `web=`, `sessions=`.

Review asks:

1. Read-only state without a login - yes/no?
2. PBKDF2 at 10 000 iterations with sessions lost at reboot - yes, or do
   sessions need to survive an OTA reboot (store them in RTC memory next to
   the marker)?
3. Released images restrict 4000/4001 to read-only by default (ADR 0004) -
   yes/no?

---

## Open Questions

**Architecture-changers**
- [ ] Do the line ports (4000/4001) get the same protection as the page
      (read-only by default, ADR 0004), or stay a documented LAN-trusted back
      door for the bench?

**Behavior definers**
- [ ] Should a `.wota` from an older firmware version be refused (downgrade
      protection), or is "signed" enough as in ADR 0002?
- [ ] Session lifetime: 12 h idle, or "until logout" for a wall tablet?
