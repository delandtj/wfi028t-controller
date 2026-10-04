# Brief: a Home Assistant card for the WFI-028T controller

Hand-off for a session that builds the Lovelace card. Everything the card
needs to know about what the controller publishes over MQTT, how commands
behave, and where the traps are. Facts are taken from the firmware as of
commit 3e6bc8b plus the numbered `last_command` change that ships with this
brief (`fw/src/mqtt.rs`, `fw/src/mqtt/{entity,json,config}.rs`,
`fw/src/poll.rs`, `fw/src/master.rs`, `hp-model/src/{status,settings}.rs`);
cites are `file:line` relative to the repo root.

Repo: `/home/delandtj/Electronics/wfi028t-controller`
(github.com/delandtj/wfi028t-controller). Read this brief first; open the
source only to confirm a detail.

The existing `docs/home-assistant-dashboard.md` is a working YAML view built
from stock cards (tile, conditional, history-graph, entities). It is the
baseline the new card has to beat, and its entity table and notes agree with
this brief.

---

## 1. What the card is for

A pool heat pump (W'Eau WFI-028T, a Tuya rebrand) whose wall controller has
been replaced by an ESP32-C6 that is Modbus master on the heat pump's RS-485
bus and publishes to Home Assistant via MQTT discovery. There is deliberately
no `climate` entity (section 8, items 1-2), so the card is what gives the user
one coherent control: power, mode, the setpoint that belongs to the current
mode, boost, stop-at-target, and the readings that tell whether the pump is
actually doing anything.

### Must

1. Power toggle, mode selector (`heat`/`cool`/`auto`), and exactly one
   setpoint control: the register that belongs to the current mode (P01 in
   heat, P02 in cool, P03 in auto), with that register's own min/max from
   its entity attributes. Never a shared range.
2. Boost and stop-at-target toggles.
3. Water in, water out, their difference (out - in), air temperature,
   compressor running/frequency, water pump, heating active.
4. One status banner, chosen by priority (section 6): controller offline >
   listen mode > Modbus link down > settings not read yet. Show only the
   highest that applies; listen mode also turns `link` off, so a naive card
   would show two.
5. Controls disabled (with the banner as the reason) in every banner state.
   Commands are refused in listen mode and fail with the link down.
6. A water flow fault alarm (`binary_sensor.wfi028t_water_flow_fault`),
   always visible when on, independent of the banner.
7. No optimistic UI: after a change, show a pending state until
   `sensor.wfi028t_last_command` reports the outcome for that command
   (matched by sequence number, section 5), then show the outcome if it is
   not `ok`.
8. Setpoint input debounced: send one `number.set_value` about 1 s after the
   last tap, with the final value. The controller takes one command per
   second into a queue of 4; five quick taps would hit `command queue full`.
9. Do not send a value equal to the entity's current state (setpoints,
   toggles, mode): nothing would change on the heat pump.
10. `select.wfi028t_mode` = `unknown`: show no setpoint control at all, not
    a default.
11. Works on a phone. Handles `unknown` / `unavailable` on every entity
    without throwing or showing `NaN`.

### Should

- Show `last_command` (small, secondary) so a refused or failed command is
  explained.
- Show the refrigerant/diagnostic values (section 3) behind an expander or a
  second card, not on the main face.
- Boost: show requested (switch) and confirmed (`binary_sensor.wfi028t_boost_active`)
  separately, or indicate when they disagree.

### Not wanted

- A thermostat dial with one range across modes.
- Mapping "off" into the mode selector (power is a separate bit).
- Any control for listen/master, bus settings or MQTT settings (console only).
- Talking to MQTT directly from the card. Go through the HA entities and
  services; the discovery config already maps them.

### Implementation choice

Left to the implementing session. Two reasonable routes:

- A custom card (`custom:wfi028t-card`, plain JS/Lit, one file under
  `www/`, registered as a dashboard resource). Best fit for "Must 7"
  (pending state and outcome matching need code).
- Stock cards plus `card-mod`/`mushroom`-style community cards, extending
  the existing YAML. Cheaper, but pending/outcome handling is weak.

Recommendation: the custom card. Calls to use from it:

| Action | HA service call |
|---|---|
| power / boost / stop at target | `switch.turn_on` / `switch.turn_off` on the switch entity |
| mode | `select.select_option` with `option: heat|cool|auto` |
| setpoint | `number.set_value` with an integer `value` |

---

## 2. MQTT interface

All QoS 0. Topic base and device id are the hard-coded `wfi028t`
(`fw/src/mqtt/entity.rs:34`); discovery prefix is `homeassistant`. Neither is
configurable.

| Topic | Dir | Retain | Payload |
|---|---|---|---|
| `wfi028t/availability` | out | yes | `online` / `offline` (also the last will) |
| `wfi028t/state` | out | yes | one flat JSON object, all 36 values (section 4) |
| `homeassistant/<component>/wfi028t/<object_id>/config` | out | yes | discovery JSON |
| `wfi028t/<object_id>/set` | in | must be no | bare value, not JSON (section 5) |
| `homeassistant/status` | in | - | HA birth; `online` triggers rediscovery + full state |

There are no per-entity state topics, no attribute topics, and no error
topic. Diagnostics are keys in the same state document.

Availability is the controller's own broker connection only. It says nothing
about the heat pump; that is `link`.

### Discovery payload shape

Every entity shares `state_topic: wfi028t/state` and picks its field with
`value_template: {{ value_json.<object_id> }}`. `unique_id` and `object_id`
are both `wfi028t_<object_id>`, so entity ids are
`<component>.wfi028t_<object_id>`. Device block, identical in all payloads:

```json
"device":{"identifiers":["wfi028t"],"name":"Pool heat pump","model":"WFI-028T","manufacturer":"W'Eau","sw_version":"<fw version>"}
```

Kind-specific fields:

| Kind | Extra fields |
|---|---|
| switch | `command_topic`, `payload_on/off` `ON`/`OFF`, `state_on/off` `ON`/`OFF` |
| select | `command_topic`, `options: ["heat","cool","auto"]` |
| number | `command_topic`, `min`, `max`, `step: 1`, `mode: "box"`, `unit_of_measurement` degC |
| sensor | optional `device_class`, `unit_of_measurement`, `state_class` |
| binary_sensor | `payload_on/off` `ON`/`OFF`, optional `device_class` |

Diagnostic entities add `entity_category: "diagnostic"`. No `icon`,
`expire_after`, `has_entity_name`, `optimistic` or `command_template` is
sent. The degree sign goes out as the JSON escape `\u00b0C`
(`entity.rs:68`), which HA decodes to the degree-C unit.

Example (switch `power`, one line on the wire):

```json
{"name":"Power","unique_id":"wfi028t_power","object_id":"wfi028t_power","state_topic":"wfi028t/state","value_template":"{{ value_json.power }}","availability_topic":"wfi028t/availability","payload_available":"online","payload_not_available":"offline","command_topic":"wfi028t/power/set","payload_on":"ON","payload_off":"OFF","state_on":"ON","state_off":"OFF","device":{...}}
```

Example (number `p02`):

```json
{"name":"Cooling setpoint","unique_id":"wfi028t_p02","object_id":"wfi028t_p02","state_topic":"wfi028t/state","value_template":"{{ value_json.p02 }}","availability_topic":"wfi028t/availability","payload_available":"online","payload_not_available":"offline","command_topic":"wfi028t/p02/set","min":8,"max":28,"step":1,"mode":"box","unit_of_measurement":"\u00b0C","device":{...}}
```

---

## 3. Entities (36; `entity.rs:203-327`)

Entity id is always `<component>.wfi028t_<object_id>`. In HA the friendly
name is prefixed with the device name ("Pool heat pump Power"); the card
should set its own short labels.

### Controls (8, writable)

| Entity id | Name | Values | Notes |
|---|---|---|---|
| `switch.wfi028t_power` | Power | on/off | Reg 0x003f bit 0 |
| `switch.wfi028t_boost` | Boost | on/off | Reg 0x003f bit 6, inverted in the register (set = ECO); the switch is already corrected: `on` = boost requested |
| `switch.wfi028t_stop_at_target` | Stop at target | on/off | P05, reg 0x003f bit 4. on = stop when target reached, off = run non-stop |
| `select.wfi028t_mode` | Mode | `heat`, `cool`, `auto` | Reg 0x0040 (1/2/7). Unknown raw value -> `unknown` |
| `number.wfi028t_p01` | Heating setpoint | 8..40, step 1 | Used in `heat` |
| `number.wfi028t_p02` | Cooling setpoint | **8..28**, step 1 | Used in `cool` |
| `number.wfi028t_p03` | Auto setpoint | 8..40, step 1 | Used in `auto` |
| `number.wfi028t_p04` | Restart hysteresis | 1..18, step 1 | A delta, though it carries the degC unit. Secondary control |

Setpoints are whole degrees.

### Temperatures (sensor, `device_class: temperature`, degC, `measurement`)

| Entity id | Name | Resolution |
|---|---|---|
| `sensor.wfi028t_inlet_water` | Inlet water temperature | 0.1 |
| `sensor.wfi028t_outlet_water` | Outlet water temperature | 0.5 |
| `sensor.wfi028t_ambient` | Ambient temperature | 0.5 |
| `sensor.wfi028t_exhaust` | Exhaust temperature | 1 (published as `76.0`) |
| `sensor.wfi028t_gas_return` | Gas return temperature | 0.5 |
| `sensor.wfi028t_outer_piping` | Outer piping temperature | 0.5 |
| `sensor.wfi028t_inner_piping` | Inner piping temperature | 0.5 |
| `sensor.wfi028t_radiator` | Radiator temperature | 0.5 |

Values can be negative. A sensor that is not fitted reads `unknown`
(raw 0x7fff -> JSON `null`).

### Other sensors (`measurement`, integers)

| Entity id | Name | Unit | device_class |
|---|---|---|---|
| `sensor.wfi028t_compressor_hz` | Compressor frequency | Hz | frequency |
| `sensor.wfi028t_compressor_target_hz` | Compressor target frequency | Hz | frequency (mapping "likely"; leads actual by a step) |
| `sensor.wfi028t_compressor_current` | Compressor current | A | current (whole amps) |
| `sensor.wfi028t_dc_bus_volts` | DC bus voltage | V | voltage |
| `sensor.wfi028t_eev_steps` | EEV aperture | steps | - |
| `sensor.wfi028t_fan_rpm` | Fan speed | rpm | - |
| `sensor.wfi028t_fan2_rpm` | Second fan speed | rpm | - (0 on a single-fan unit) |

### Binary sensors

| Entity id | Name | device_class | Meaning |
|---|---|---|---|
| `binary_sensor.wfi028t_water_flow_fault` | Water flow fault | problem | The only alarm bit mapped. Self-clears |
| `binary_sensor.wfi028t_compressor_running` | Compressor running | running | Derived: compressor_hz > 0 |
| `binary_sensor.wfi028t_water_pump` | Water pump | - | mapping "likely" |
| `binary_sensor.wfi028t_heating_active` | Heating active | - | mapping "candidate" |
| `binary_sensor.wfi028t_run_permitted` | Run permitted | - | Clears at power-off and during a flow fault |
| `binary_sensor.wfi028t_boost_active` | Boost active | - | Heat pump's confirmation of the boost request; may lag the switch |
| `binary_sensor.wfi028t_link` | Modbus link | connectivity | Diagnostic. Controller <-> heat pump bus link |

### Diagnostic sensors (`entity_category: diagnostic`)

| Entity id | Name | Content |
|---|---|---|
| `sensor.wfi028t_controller_mode` | Controller mode | `listen` or `master` |
| `sensor.wfi028t_last_command` | Last command | Text, max 96 chars, e.g. `#17 p01 34 -> ok` (section 5) |
| `sensor.wfi028t_requests` | Bus requests | `total_increasing`, about +2/s |
| `sensor.wfi028t_timeouts` | Bus timeouts | `total_increasing` |
| `sensor.wfi028t_writes` | Block writes | `total_increasing` |
| `sensor.wfi028t_write_failures` | Failed commands | `total_increasing`; bus-side failures only |

Counters reset at controller reboot and can be up to 60 s stale.

### Not available

No defrost state, no general fault flag, no error codes. Do not design UI
around them.

### Entity id caveat

HA reads `object_id` only when it first registers an entity. An install that
predates the `object_id` field may have different ids. Card config: one
`prefix` (default `wfi028t`, giving `<component>.<prefix>_<object_id>`) plus
an optional `entities:` map of per-entity overrides keyed by object_id, e.g.
`entities: { p01: number.old_heating_setpoint }`. No 36-key config.

---

## 4. State document (`fw/src/mqtt/json.rs:96-195`)

One flat object, 36 keys in fixed order, no whitespace, on `wfi028t/state`
(retained):

```json
{"power":"ON","boost":"OFF","stop_at_target":"ON","mode":"heat","p01":33,"p02":27,"p03":27,"p04":1,"inlet_water":27.5,"outlet_water":29.5,"ambient":20.0,"exhaust":76.0,"gas_return":7.0,"outer_piping":6.0,"inner_piping":32.5,"radiator":40.0,"eev_steps":130,"compressor_current":8,"dc_bus_volts":541,"compressor_hz":54,"compressor_target_hz":55,"fan_rpm":722,"fan2_rpm":0,"water_flow_fault":"OFF","boost_active":"OFF","water_pump":"ON","heating_active":"ON","run_permitted":"ON","compressor_running":"ON","link":"ON","controller_mode":"master","last_command":"#17 p01 34 -> ok","requests":1234,"timeouts":2,"writes":3,"write_failures":1}
```

Encoding:
- On/off values are the strings `"ON"`/`"OFF"`, never booleans (HA maps them
  to `on`/`off`).
- Temperatures have exactly one decimal; setpoints and other sensors are
  integers.
- `null` means unknown. HA shows it as `unknown`.
  - Before the first settings read: `power`, `boost`, `stop_at_target`,
    `mode`, `p01`-`p04` are null.
  - Before the first status read: every sensor and status bit is null.
  - `link`, `controller_mode`, `last_command` and the counters are never
    null.

When it is published (`fw/src/mqtt.rs:694-780`):
- Only when something other than the counters changed (it compares the
  document without its counter tail). A quiet heat pump produces no traffic;
  a fluctuating sensor can cause about 2 publishes per second.
- After every command outcome and every MQTT-layer refusal. Each one gets
  a new sequence number, so the document always changes and is always
  published.
- Every 60 s, forced.
- On connect and when HA sends `homeassistant/status` = `online`.

---

## 5. Commands

### Payloads (`entity.rs:471-528`)

The card goes through HA services, which send these for it:

| Entity | Accepted on `wfi028t/<id>/set` |
|---|---|
| power, boost, stop_at_target | `ON` / `OFF` (case-insensitive) |
| mode | `heat` / `cool` / `auto` (case-insensitive) |
| p01-p04 | whole number; `34`, `34.0` and `34.` all accepted; `34.5` refused |

The payload is not trimmed. The MQTT parser checks syntax only. The range
check happens on the bus side, so `p02 = 30` is accepted at MQTT and then
rejected by the model (outcome below). The card must therefore clamp to each
number's own `min`/`max` attributes.

### Path and timing

1. HA publishes the command; the controller queues it (`COMMANDS`, capacity
   4). Nothing is echoed back.
2. The bus master takes one command per 1 s cycle, right after a fresh
   settings read (settings older than 2 s are refused). It writes the whole
   settings block three times over three 500 ms slots, then verifies with
   the next settings read.
3. The entity's own state usually changes within about 1-2 s, when the
   settings poll sees the new value.
4. `last_command` gets the outcome in about 2.5-3.5 s. An MQTT-layer
   refusal (section below) lands at once, well under a second.

A card pending timeout of about 8 s is reasonable; after it, show "no
response". Nothing short of a lost MQTT message or a controller reboot
should get there.

### `last_command` format

`#<seq> <command> -> <outcome>[ (<detail>)]`, clipped to 96 chars including
the number. `<seq>` is a decimal counter starting at 1 for the first report
after boot and increasing by one for every bus outcome and every MQTT-layer
refusal. It survives MQTT reconnects and resets on reboot (back to 1; the
value is `""` until the first report). Only the most recent report is kept.

Command spellings (`fw/src/master.rs:314-327`): `power on`, `power off`,
`boost on`, `boost off`, `p05 on`, `p05 off` (that is stop_at_target),
`mode heat`, `mode cool`, `mode auto`, `p01 34`, `p02 ..`, `p03 ..`, `p04 ..`.

Bus outcomes:

| Outcome | Meaning |
|---|---|
| `ok` | written, acknowledged, verified on readback |
| `rejected` | model refused the value; detail like `(30 outside 8..=28)` or `(mode 0x.... not supported)` |
| `no-fresh-settings` | Modbus link down or settings stale; nothing written |
| `no-ack` | none of the three writes was acknowledged |
| `readback-mismatch` | acknowledged but not seen in the next settings read |
| `not-master` | controller left master mode before or during the write |
| `rebooting` | firmware update reboot in progress |

Refusals at the MQTT layer use the same format, with the entity and raw
payload (max 24 chars) in place of the command, e.g.
`#18 mode warm -> expected heat, cool or auto`.
Reasons:
`payload is not UTF-8`, `retained command ignored`, `unknown entity`,
`entity is read-only`, `expected ON or OFF`, `expected heat, cool or auto`,
`expected a whole number of degrees`, `fractional degrees are not settable`,
`controller is in listen mode`, `command queue full`. They are published
immediately and are not counted in `write_failures`. Note the spelling
differs from bus outcomes: a refusal names the entity object_id and payload
(`stop_at_target ON`), a bus outcome names the command (`p05 on`).

### Matching an outcome to the click

1. Before sending, remember `N` = the current `<seq>` (0 if `last_command`
   is empty or unknown).
2. Send. Every report from then on has `<seq>` > `N`, and each changes the
   state of `sensor.wfi028t_last_command`, so HA fires a state change even
   when the text after the number repeats.
3. Take the first report with `<seq>` > `N` whose text matches the command:
   bus spelling (`p01 34`, `power on`, `boost off`, `p05 on`, `mode cool`)
   or refusal spelling (`p01 34`, `power ON`, `stop_at_target ON`,
   `mode cool`; case as sent by HA). For setpoints compare the number, not
   the text: HA may send `34.0`, so a refusal can read `p01 34.0`. Reports
   for other commands, from another client or an earlier click, are
   skipped.
4. If `<seq>` goes down (controller rebooted), drop the pending state and
   re-read.

The entity's own state change is a good early "it worked", but the outcome
line is authoritative: a value can also change because someone used the
heat pump's own panel.

---

## 6. Failure modes the card must render

Banner priority, highest first; show only the top one that applies:

1. Controller offline (entities `unavailable`)
2. Listen mode (`controller_mode` = `listen`; `link` is also off)
3. Modbus link down (`link` = `off`)
4. Settings not read yet (switches/select/numbers `unknown`)

The water flow fault alarm is separate and shows whenever it is on, except
in state 1 where nothing is known.

| Situation | What HA sees | Card should |
|---|---|---|
| Controller lost the broker | all 36 entities `unavailable` (last will) | Show "controller offline", disable everything |
| Modbus link down (master mode) | `binary_sensor.wfi028t_link` = `off`; all other values frozen at the last read, still available | Banner "no link to heat pump, values stale"; disable controls (commands would fail with `no-fresh-settings`) |
| Listen mode | `controller_mode` = `listen`; `link` off; values null unless it was master earlier this boot | Show "listen-only (console: `mode master`)"; disable controls |
| Settings not read yet | switches/select/numbers `unknown` | Placeholder, controls disabled |
| Mode unknown (unsupported raw value) | `select.wfi028t_mode` = `unknown`, other controls fine | No setpoint control; power, boost, stop at target still usable |
| Sensor not fitted | that sensor `unknown` | Show `-`, not 0 |
| Water flow fault | `water_flow_fault` = `on`; `run_permitted` goes off | Prominent alarm |
| HA restarted, controller offline | retained state doc is replayed, then availability `offline` | Trust availability over values |

Retained-state note: `wfi028t/state` is retained, so HA gets the last known
document at startup even if the controller is gone; availability is what
decides.

---

## 7. Configuration facts (not needed in the card, for context)

Broker set at build time (`MQTT_HOST` as an IPv4 literal, `MQTT_PORT`,
`MQTT_USER`, `MQTT_PASS`) or at runtime on the console (TCP 4001):
`mqtt host <ip> [port]`, `mqtt user <name> <password>`, `mqtt off`, `mqtt`
to report. Runtime settings persist in flash. Reconnect backoff 2 s -> 60 s;
keepalive 60 s; on reconnect it republishes availability, all discovery, then
the state document.

---

## 8. Traps, collected

1. **Ranges differ per mode.** P02 (cool) is 8..28, P01/P03 are 8..40. A
   single dial would offer 35 in cool; the bus rejects it and the UI sees
   no change. Use each number's own `min`/`max` attributes.
2. **Off is power, not a mode.** `mode` stays `heat`/`cool`/`auto` while
   power is off. Show mode and setpoint dimmed when off, still editable.
3. **Boost**: the switch is the request (from settings); `boost_active` is
   the heat pump's confirmation and lags. Inversion is already handled.
4. **Stop at target** is P05 and shows as `p05` in `last_command`.
5. **No optimistic state.** Changes appear after the readback, 1-3 s
   (section 5).
5a. **Queue of 4, one command per second.** Debounce setpoint taps; never
   fire one call per tap.
6. **Stale values on link loss** are not nulled. Do not trust a value while
   `link` is `off`.
7. **Availability is not the heat pump.** It is only the controller to broker
   link.
8. **Listen mode** refuses every command and is not switchable from HA.
9. **Counters** are `total_increasing`, reset at reboot, up to 60 s stale.
   Show rates/statistics, not raw totals, if at all.
10. **Temperatures**: inlet is in tenths, the others in halves; exhaust in
    whole degrees. Show one decimal.
11. **delta-T** (outlet - inlet) is the most useful single number for "is
    it heating"; compute it in the card (both `unknown` -> show `-`).
12. **Mapping confidence.** `water_pump` ("likely"), `heating_active`
    ("candidate"), `run_permitted` ("likely") and `compressor_target_hz`
    ("likely") are not fully confirmed (`docs/register-map.md`). Label
    them neutrally; do not drive critical UI (alarms) from them.
13. **Code comment drift**: `write_discovery`'s doc says 30 entities; the real
    count is 36 (asserted at `entity.rs:573`).

---

## 9. Acceptance checks

With the controller in master mode and the link up:

- [ ] Changing mode swaps the setpoint control to P01/P02/P03 with that
      register's range; P02 cannot be set above 28 from the card.
- [ ] A setpoint change shows pending, then the new value within about 3 s;
      `last_command` reads `#<n> pNN <v> -> ok`.
- [ ] Five quick taps on +1 send one command with the final value.
- [ ] Setting the value the setpoint already has sends nothing.
- [ ] `p02 30` sent twice shows the rejection both times, not "no
      response" the second time. Publish it directly
      (`mosquitto_pub -t wfi028t/p02/set -m 30`): HA's `number.set_value`
      checks min/max itself and raises `ServiceValidationError` before
      anything reaches MQTT, so Developer Tools > Actions cannot provoke a
      bus-side rejection. The card shows that HA error as the outcome.
- [ ] Power, boost and stop-at-target toggles show pending, then the new
      state; boost shows requested vs confirmed.
- [ ] delta-T shows and updates; `-` when either temperature is unknown.

Failure paths (simulate by editing entity states in Developer Tools >
States, or by stopping the broker / unplugging the bus):

- [ ] `binary_sensor.wfi028t_link` = `off` -> stale banner, controls
      disabled.
- [ ] `sensor.wfi028t_controller_mode` = `listen` (and `link` off) -> only
      the listen-only banner, controls disabled.
- [ ] `select.wfi028t_mode` = `unknown` -> no setpoint control.
- [ ] All entities `unavailable` -> "controller offline", no errors in the
      browser console.
- [ ] Any single entity `unknown` -> placeholder, no `NaN`.
- [ ] `water_flow_fault` = `on` -> alarm visible on the main face.
- [ ] `prefix` and per-entity `entities:` overrides both work.
