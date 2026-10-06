# WFI-028T/035T register map

Register map for the Modbus RTU bus between the controller and the
WFI-028T/035T full-inverter pool heat pump (380-415V/3N), as used by the
firmware in this repository. Every entry says where it comes from.

Sources, in order of trust:

1. **Capture** - the bus itself: /var/lib/modbus-sniffer (stock controller,
   2026-10-03..04, recorded with ../stm32-modbus-sniffer; the real capture
   starts at the note "CAPTURE START real bus", 2026-10-03 14:51) and
   /var/lib/wfi-controller (this firmware as master, from 2026-10-04).
2. **Vendor protocol** -
   [`vendor/modbus-protocol-zhike-inverter-pool-heat-pump-2022-03-29-en.pdf`](vendor/modbus-protocol-zhike-inverter-pool-heat-pump-2022-03-29-en.pdf),
   transcribed in [`.md`](vendor/modbus-protocol-zhike-inverter-pool-heat-pump-2022-03-29-en.md):
   the heat pump mainboard's own protocol (Zhike, 2022-03-29, RCDF211268).
   Every register of both blocks and the bit meanings of the flag
   registers. Checked against 52 h of capture (see "Vendor document vs
   capture"): it agrees with the bus on every status bit we could test, is
   wrong about boost, gives no scaling, and calls two registers we use
   "reserved".
3. **Manual** - [`vendor/wfi-028t-035t-manual.pdf`](vendor/wfi-028t-035t-manual.pdf):
   user parameters P01-P05, running values A01-A14, error codes E04..E46.

## Bus

| Item | Value | Source |
|---|---|---|
| Physical | RS-485 2-wire: controller cable green = A, yellow = B (red/black = 12 V supply) | capture, [wiring sheet](wiring.html) |
| Framing | Modbus RTU, 9600 baud, 8N1, CRC low byte first | capture + vendor |
| Roles | controller = master, heat pump PCB = slave 0x01 (1-16 by DIP switches 1-4) | capture + vendor |
| Functions | 0x03 read, 0x06 write single, 0x10 write multiple; the stock controller only uses 0x03 and 0x10 | capture + vendor |
| Errors | no exception responses: a rejected write gets no answer and times out | vendor |
| Scaling | mixed: most temperatures raw/2, inlet water raw/10, discharge whole degrees (see "Sensors") | capture + A01-A14 readout |

## Controller behaviour

Steady state of the stock controller, one cycle per second (capture). The
firmware does the same.

| Offset | Frame |
|---|---|
| +0 ms | `01 03 0000 003f` - read 63 registers 0x0000-0x003e (status block) |
| ~+150 ms | heat pump reply, 126 data bytes |
| +500 ms | `01 03 003f 0043` - read 67 registers 0x003f-0x0081 (settings block) |
| ~+660 ms | heat pump reply, 134 data bytes |

On a settings change the controller writes the WHOLE settings block
(`01 10 003f 0043 86 <134 bytes>`) three times, 500 ms apart, each acked by
the heat pump ~24 ms later with `01 10 003f 0043`. The changed value is
visible in the next poll. The firmware writes the same way, starting from a
settings block it read moments before.

The stock controller also writes the whole block about 3 s after it boots
(breaker test 2026-10-04, see "Observed sequences"). The heat pump keeps its
settings over a power cut: its first settings reply after power-on already
held the full block, ECO bit included, and the boot write sent identical
values. A replacement does not need a boot write; the firmware does none. The
stock controller has no supply of its own: it is fed from the heat pump, so
both go down and come up together.

Only one master on the bus: the stock controller is unplugged while this
firmware is master (WFI's installation sheet says the same).

## Controls (settings block)

0x003f is the main bit field: bit 0 on/off, bit 4 P05, bit 6 ECO/boost.
Setpoints are whole degrees (unlike the sensors).

| Control | Register | Values | HA entity | Evidence |
|---|---|---|---|---|
| Power | 0x003f bit 0 | 1 = on | `switch.wfi028t_power` | capture 15:52:40 off, 15:53:15 on; vendor "wire controller ON/OFF" |
| Boost (full power) | 0x003f bit 6, inverted | 1 = ECO (vendor: silent mode), 0 = boost | `switch.wfi028t_boost` | capture, four toggles. The vendor's 0x0040 bit 4 never changes |
| Stop at target (P05) | 0x003f bit 4 | 1 = stop once the target is reached | `switch.wfi028t_stop_at_target` | capture via the P menu; vendor "water pump mode: 0 continuous, 1 periodic" |
| Mode | 0x0040 | 1 heating, 2 cooling, 7 auto | `select.wfi028t_mode` | vendor; capture shows 1 in heating |
| P01 heating setpoint | 0x0041 | 8-40 C | `number.wfi028t_p01` | capture 33->34->33 |
| P02 cooling setpoint | 0x0042 | 8-28 C | `number.wfi028t_p02` | capture 27->28->27 |
| P03 auto setpoint | 0x004a | 8-40 C | `number.wfi028t_p03` | capture 27->28->27 |
| P04 restart hysteresis | 0x004d | 1-18 C | `number.wfi028t_p04` | capture 1->2->1 |
| Forced defrost | 0x003f bit 14 | - | not exposed | vendor only; never written. Candidate for a manual defrost button |

## Other settings

The rest of the settings block, named by the vendor protocol. Constant since
the start of the capture (see the snapshot at the end); where the vendor gives
a default it matches the captured value. None of these has been written. They
are installer/factory protection parameters: the names make a change
targeted, not safe.

| Registers | Captured | Vendor name (range, default) |
|---|---|---|
| 0x0043 | 50 | manual frequency setting |
| 0x0044 | 150 | manual EXV step position (20-450, 300) |
| 0x0045-0x0048 | -1, -1, 0x7fff, -1 | manual aux valve, manual frequency 2, manual EXV 2, manual aux valve 2 (second circuit, not fitted) |
| 0x0049 | 500 | manual fan speed |
| 0x004b, 0x004f, 0x005c | 10, -20, 23 | reserved |
| 0x004c | -1 | mode changeover time (3-30 min, 10) - not set on this unit |
| 0x004e | 0 | inlet water temperature compensation |
| 0x0050-0x0055 | 40, -6, 11, 16, 6, 17 | defrost: interval 20-90 min, start temp -15..-1 C, duration 5-20 min, termination temp 1-40 C, ambient-to-coil difference 0-15 C, ambient threshold 0-20 C (all at the vendor defaults) |
| 0x0056-0x0058 | 30, 1, 88 | EXV adjustment period 20-90 s, heating target superheat -5..10 C, EXV discharge-temperature target 70-125 C |
| 0x0059, 0x005a | 40, 8 | EXV opening during defrost (20-450, 400), minimum EXV opening (50-150, 80): stored in units of 10 steps |
| 0x005b | 1 | cooling target superheat (-5..10 C) |
| 0x005d-0x0066 | 40 44 48 54 58 64 72 80 84 90 | compressor frequency ladder F1-F10 (30-90 Hz); boost ran at 80, settled at 72 |
| 0x0067-0x006b | 95 100 105 110 115 | discharge temperature thresholds TP0-TP4 (50-125 C) |
| 0x006c-0x006e | 12, 13, 14 | reserved |
| 0x006f-0x0074 | 46 52 58 64 72 85 | fan speed levels 1-6 (20-100) |
| 0x0075, 0x0076 | 0, 1 | fan speed level selection (0-6), fan type (0 AC, 1 DC, 2 EC) |
| 0x0077, 0x0078 | 12, -1 | reserved |
| 0x0079 | 0 | timer enable flags |
| 0x007a-0x0081 | 8,0, 12,0, 14,0, 17,0 | timer 1 on/off, timer 2 on/off (hour, minute): 08:00-12:00 and 14:00-17:00 |
| 0x0082 | - | vendor "control switch" flags, RW; outside the block the stock controller reads |

## Sensors (status block)

Mapped 2026-10-03 ~15:40 by reading A01-A14 on the controller (hold [-] 3 s,
step with [+]/[-], short press [power] to exit) and matching each value to the
status block polled at that moment. The readouts are in the capture log as
"Axx ... (user readout)" notes. The menu triggers no extra bus requests.

**Scaling is mixed.** Most temperatures are half degrees (raw / 2); inlet
water is tenths (raw / 10); discharge is whole degrees. Do not assume a single
scale for new registers. The vendor protocol gives no scaling at all.

| Sensor | Register | Raw at readout | Scaling | Display | HA entity (`sensor.wfi028t_...`) |
|---|---|---|---|---|---|
| A01 inlet water | 0x000f | 275 | / 10 | 27.5 C | `inlet_water` |
| A02 outlet water | 0x0010 | 59 | / 2 | 29.5 C | `outlet_water` |
| A03 ambient | 0x0011 | 41 | / 2 | 20.5 C | `ambient` |
| A04 exhaust (vendor: discharge 1) | 0x0015 | 76 | x 1 | 76 C | `exhaust` |
| A05 gas return (vendor: suction gas 1) | 0x0013 | 14 | / 2 | 7.0 C | `gas_return` |
| A06 outer piping (vendor: coil 1) | 0x0012 | 12 | / 2 | 6.0 C | `outer_piping` |
| A07 inner piping (vendor: cooling coil 1) | 0x0014 | 64 | / 2 | 32.0 C | `inner_piping` |
| A08 EEV aperture (vendor: main EXV opening 1) | 0x0018 | 125 | x 1 | 125 steps | `eev_steps` |
| A09 compressor current | 0x0020 | 8 | x 1 | 8 A | `compressor_current` |
| A10 radiator temp. | 0x001f | 80 | / 2 | 40.0 C | `radiator` |
| A11 DC bus voltage (~1.35 x 400 V AC) | 0x001e | 544 | x 1 | 544 V | `dc_bus_volts` |
| A12 compressor frequency (actual) | 0x001b | 54 | x 1 | 54 Hz | `compressor_hz` |
| A13 fan speed | 0x0024 | 724 | x 1 | 724 r/min | `fan_rpm` |
| A14 second fan speed | 0x0025 | 0 | x 1 | 0 (single-fan unit) | `fan2_rpm` |
| Compressor target frequency | 0x001a | 55 | x 1 | 55 Hz | `compressor_target_hz` |

0x0020 and 0x001f are "reserved" in the vendor protocol. 0x0020 behaves as a
current (0 when stopped, ~0.14 x Hz running); 0x001f sits at 80 and only moves
to 82-86 near 79 Hz, so the radiator reading is weak.

Second circuit (vendor: coil 2, suction gas 2, EXV 2, compressor 2, inverter 2
at 0x0026-0x0032) reads 0 or 0x7fff: not fitted on this single-compressor
unit. Inverter module 1 fault codes 0x001c/0x001d have been 0 throughout.

## Status and alarms

| Signal | Register | HA entity (`binary_sensor.wfi028t_...`) | Evidence |
|---|---|---|---|
| Boost active / high fan speed | 0x0004 bit 7 | `boost_active` | vendor: high/low fan speed; mirrors 0x003f bit 6 in 99.99% of samples |
| Heating demand | 0x0005 bit 7 | `run_permitted` (name: Heating demand) | vendor: AC heating demand; off at thermostat stops and power-off |
| Water pump | 0x0006 bit 2 | `water_pump` | vendor: circulating water pump; on with the unit, off ~45 s after a compressor stop |
| Fan output | 0x0004 bit 5 | not published | vendor: fan; leads fan rpm > 0 by 5-6 s, 99.98% agreement |
| Compressor output | 0x0004 bit 0 | `heating_active` (name: Compressor output) | vendor: compressor 1; set with the target frequency, 9-12 s before the compressor turns |
| Compressor running | 0x001b > 0 | `compressor_running` | derived |
| Water flow fault | 0x0008 bit 0 | `water_flow_fault` | vendor: water flow switch fault; set 10 s into the flow test |
| Crankcase heater | 0x0006 bit 1 | not published | vendor; on at night with the compressor off |
| Defrosting | 0x0003 bit 7, 0x0004 bit 6 (four-way valve) | not published | vendor; never set in 52 h of capture (no cold-weather run yet) |
| Any fault | 0x0007-0x000d nonzero, 0x001c/0x001d | not published | vendor bit list in the transcription; only 0x0008 bit 0 seen so far |
| Thermostat stop flag | 0x0002 bit 1 | not published | capture only: sets at every compressor stop, clears at the next start or a morning reset; not in the vendor document. The firmware read it as the flow fault until 2026-10-06 |

Error codes from the manual, for a future error-text sensor: water flow
protection, E04 antifreeze, E05 high pressure, E06 low pressure, E09
controller-PCB link, E10 PCB-driver link, E12 exhaust temp. too high, E15
inlet water sensor, E16 outer piping sensor, E18 exhaust sensor, E20 inverter
module, E21 ambient sensor, E23 overcooling (cooling mode), E27 outlet water
sensor, E29, E33, E42, E46 DC fan motor (see the manual for the full text).

Left to Home Assistant: the clock and the timer schedules (automations).

## Vendor document vs capture

The vendor protocol checked against all captures 2026-10-03..06 (52.7 h,
7 compressor cycles, 1 s samples). Where the two disagree the capture wins.

| Item | Vendor document | Capture | Verdict |
|---|---|---|---|
| Framing, slave | 9600 8N1, slave 1-16 by DIP switches 1-4 | 9600 8N1, slave 0x01 | agree; other units may sit at another address |
| 0x06 write | echo on success, otherwise no response | stock controller only uses 0x10 | no exception responses: a bad write times out |
| Temperatures 0x000f-0x0015, 0x0026-0x0029 | names and ranges, no scaling | /10 inlet, /2 most, x1 discharge | agree; scaling is ours |
| 0x001a, 0x001b | compressor 1 target / actual frequency | target leads actual | agree |
| 0x001c, 0x001d | inverter module 1 fault codes | always 0 | unobserved |
| 0x001f | reserved | 80 (A10 radiator 40 C), moves 80-86 only at ~79 Hz | keep ours, weak |
| 0x0020 | reserved | 0 when stopped, ~0.14 x Hz running (A09) | ours: compressor current |
| 0x0002 | control switch flags (bit 6 water flow) | bit 1 at thermostat stops, bit 6 never | vendor table looks misplaced (printed under 0x0082); bit 1 unexplained |
| 0x0003 | work status (bit 0 hot water, 2 heating, 3 cooling, 7 defrost) | constant 0x000d | not usable as a mode indication |
| 0x0004 | bit 0 compressor 1, 5 fan, 6 four-way valve, 7 high fan speed | as stated; bit 6 never set | vendor right |
| 0x0005 bit 7 | AC heating demand | follows the compressor demand | vendor right |
| 0x0006 | bit 1 crankcase heater, bit 2 water pump | bit 1 at night with the compressor off, bit 2 with the pump | vendor right; bit 4 (unlisted) tracks the compressor |
| 0x0008 bit 0 | water flow switch fault | set during the flow test | vendor right |
| 0x003f bit 4 | water pump mode: 0 continuous, 1 periodic | P05 (1 = stop at target) | same switch, two names |
| 0x003f bit 6 | silent mode | 1 = ECO, 0 = boost | same switch; ECO is silent mode |
| 0x003f bits 13, 14 | fan mode, forced defrost | 0 throughout | untested |
| 0x0040 bit 4 | boost on/off | never changes when boost is toggled | vendor wrong |

## Observed sequences

Raw status bits as captured; the meanings are those of "Status and alarms".

Off/on 2026-10-03 (restart delay of a few minutes is normal for this unit):

| Time | Event | Status bits | Compressor / fan |
|---|---|---|---|
| 15:52:40 | off written (0x003f bit 0 = 0) | compressor output (0x0004 bit 0) off, heating demand (0x0005 bit 7) off | ramping down from 54 Hz |
| 15:53:15 | on written | heating demand on | 22 Hz, ramping down |
| 15:54:01 | compressor stopped | fan output (0x0004 bit 5) off | 0 Hz, 0 A, fan spinning down |
| 15:56:15 | restart: fan output on | 0x0004 bit 5 on | EEV to 300 (start position) |
| 15:56:20 | | compressor output on | |
| 15:57:32 | boost set by user, compressor running | 0x0004 bit 7 on | 41 Hz, fan 728, 7 A |

Water flow test 2026-10-03 (user stopped the pool circulation pump, unit on,
heating, ECO):

| Time | Event | Status bits | Compressor / water |
|---|---|---|---|
| 16:57:13 | circulation stopped (note) | | 54 Hz |
| 16:57:15 | | 0x0002 bit 1 on (thermostat stop flag) | 54 Hz |
| 16:57:25 | flow fault | compressor output off, heating demand off, 0x0008 bit 0 on (water flow fault) | 54 Hz, 10 A |
| 16:58:10 | compressor stopped | fan output off, 0x0006 0x0014 -> 0 (water pump off) | 0 Hz; outlet rises to 39 C (stagnant) |
| 17:01:37 | circulation back | 0x0002 bit 1 off | |
| 17:01:38 | fault cleared | heating demand on, 0x0006 -> 0x0014, 0x0008 bit 0 off | |
| 17:03:37 | restart sequence (~2 min) | fan output on | EEV 300 |
| 17:03:42 | | compressor output on | then 41 Hz |

The fault self-clears; no user action or power cycle needed.

Breaker test 2026-10-04 (heat pump breaker off ~61 s; the sniffer is on
another circuit; unit on, heating):

| Time | Event | Status bits | Compressor / other |
|---|---|---|---|
| 15:14:51 | user sets ECO; block written 3x | 0x003f 0x1031 -> 0x1071, 0x0004 0xa1 -> 0x21 | target 80 Hz, then -5 Hz every 5 s |
| 15:15:07.8 | breaker off | heat pump reply cut mid-frame (BAD_CRC), then the bus is silent: controller unpowered too | target 65 Hz |
| 15:16:09 | breaker on, controller polling again within ~1 s | all status 0, heating demand off | 0 Hz, fan 0; inlet temp reads 1.3 C and climbs to the real 27 C over ~7 s (filter) |
| 15:16:10-24 | | | EEV homing: 21 -> 546 -> 300 (start position) |
| 15:16:09.9 | first settings reply after power-on: full block, ECO set (heat pump kept it) | 0x003f 0x1071 | |
| 15:16:12 | controller writes the same block 3x | | |
| 15:16:39 | heating demand (+30 s) | 0x0005 bit 7 on, 0x0006 -> 0x0014 (water pump on) | |
| 15:18:39 | fan output (+2:00) | 0x0004 bit 5 on | |
| 15:18:44 | compressor output | 0x0004 bit 0 on | fan starts, target ramps 5 Hz / 5 s |
| 15:18:56 | compressor running | | 15 Hz |
| 15:19:25 | start hold | | target held at 41 Hz |
| 15:19:45 | user sets boost; block written 3x | 0x0004 bit 7 on | hold continues |
| 15:22:31 | hold ends (~3.5 min after start) | | ramps in pairs of 5 Hz steps (5 s apart), ~1 min between pairs |
| 15:24:45 | settled | | target 72, actual 71 Hz, 10 A, fan ~840 |

## Open questions

- What sets 0x0002 bit 1? It sets at every thermostat stop and clears at the
  next start; the vendor document does not list it.
- Defrost and faults: no defrost and only one fault (flow) in the capture so
  far. Wait for a cold-weather run to confirm 0x0003 bit 7 / 0x0004 bit 6 and
  the fault flags.
- Forced defrost (0x003f bit 14): does setting it start a defrost? A
  supervised test, unit running, with a way to clear it.
- Where does ECO settle? The capture never ran ECO long enough to see the
  target leave the start hold.
- 0x0082 (vendor "control switch" flags, RW) is outside the polled block:
  read it once in a polling gap to see what it holds.

## Snapshot: status block 0x0000-0x003e

Last value at 2026-10-03 15:32, plus the range over 4921 polls since 14:51.

| Addr | Dec | Last hex | u16 | s16 | /10 | Range in capture | Note |
|---|---|---|---|---|---|---|---|
| 0x0000 | 0 | 0x2020 | 8224 | 8224 | 822.4 | constant |  |
| 0x0001 | 1 | 0x0404 | 1028 | 1028 | 102.8 | constant |  |
| 0x0002 | 2 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0003 | 3 | 0x000d | 13 | 13 | 1.3 | constant |  |
| 0x0004 | 4 | 0x0021 | 33 | 33 | 3.3 | 33..161 (2 values) |  |
| 0x0005 | 5 | 0x0080 | 128 | 128 | 12.8 | constant |  |
| 0x0006 | 6 | 0x0014 | 20 | 20 | 2 | constant |  |
| 0x0007 | 7 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0008 | 8 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0009 | 9 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000a | 10 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000b | 11 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000c | 12 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000d | 13 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x000e | 14 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x000f | 15 | 0x0113 | 275 | 275 | 27.5 | 274..275 (2 values) |  |
| 0x0010 | 16 | 0x003b | 59 | 59 | 5.9 | 58..60 (3 values) |  |
| 0x0011 | 17 | 0x0028 | 40 | 40 | 4 | 39..42 (4 values) |  |
| 0x0012 | 18 | 0x000c | 12 | 12 | 1.2 | 8..14 (7 values) |  |
| 0x0013 | 19 | 0x000e | 14 | 14 | 1.4 | 11..22 (12 values) |  |
| 0x0014 | 20 | 0x0041 | 65 | 65 | 6.5 | 59..69 (11 values) |  |
| 0x0015 | 21 | 0x004c | 76 | 76 | 7.6 | 72..79 (8 values) |  |
| 0x0016 | 22 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0017 | 23 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0018 | 24 | 0x0082 | 130 | 130 | 13 | 123..152 (22 values) |  |
| 0x0019 | 25 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x001a | 26 | 0x0037 | 55 | 55 | 5.5 | 55..72 (18 values) |  |
| 0x001b | 27 | 0x0036 | 54 | 54 | 5.4 | 54..72 (16 values) |  |
| 0x001c | 28 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x001d | 29 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x001e | 30 | 0x021d | 541 | 541 | 54.1 | 537..550 (5 values) |  |
| 0x001f | 31 | 0x0050 | 80 | 80 | 8 | constant |  |
| 0x0020 | 32 | 0x0008 | 8 | 8 | 0.8 | 8..10 (3 values) |  |
| 0x0021 | 33 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0022 | 34 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0023 | 35 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0024 | 36 | 0x02d2 | 722 | 722 | 72.2 | 706..859 (35 values) |  |
| 0x0025 | 37 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0026 | 38 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0027 | 39 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0028 | 40 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0029 | 41 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x002a | 42 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x002b | 43 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x002c | 44 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x002d | 45 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x002e | 46 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x002f | 47 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0030 | 48 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0031 | 49 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0032 | 50 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0033 | 51 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0034 | 52 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0035 | 53 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0036 | 54 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0037 | 55 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0038 | 56 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0039 | 57 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003a | 58 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003b | 59 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003c | 60 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003d | 61 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x003e | 62 | 0x77ff | 30719 | 30719 | 3071.9 | constant |  |

## Snapshot: settings block 0x003f-0x0081

Same window. The two changing entries in 0x003f are the full power toggles.

| Addr | Dec | Last hex | u16 | s16 | /10 | Range in capture | Note |
|---|---|---|---|---|---|---|---|
| 0x003f | 63 | 0x1071 | 4209 | 4209 | 420.9 | 4145..4209 (2 values) |  |
| 0x0040 | 64 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x0041 | 65 | 0x0021 | 33 | 33 | 3.3 | constant |  |
| 0x0042 | 66 | 0x001b | 27 | 27 | 2.7 | constant |  |
| 0x0043 | 67 | 0x0032 | 50 | 50 | 5 | constant |  |
| 0x0044 | 68 | 0x0096 | 150 | 150 | 15 | constant |  |
| 0x0045 | 69 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0046 | 70 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0047 | 71 | 0x7fff | 32767 | 32767 | 3276.7 | constant | 0x7fff (likely 'not present') |
| 0x0048 | 72 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0049 | 73 | 0x01f4 | 500 | 500 | 50 | constant |  |
| 0x004a | 74 | 0x001b | 27 | 27 | 2.7 | constant |  |
| 0x004b | 75 | 0x000a | 10 | 10 | 1 | constant |  |
| 0x004c | 76 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x004d | 77 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x004e | 78 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x004f | 79 | 0xffec | 65516 | -20 | -2 | constant |  |
| 0x0050 | 80 | 0x0028 | 40 | 40 | 4 | constant |  |
| 0x0051 | 81 | 0xfffa | 65530 | -6 | -0.6 | constant |  |
| 0x0052 | 82 | 0x000b | 11 | 11 | 1.1 | constant |  |
| 0x0053 | 83 | 0x0010 | 16 | 16 | 1.6 | constant |  |
| 0x0054 | 84 | 0x0006 | 6 | 6 | 0.6 | constant |  |
| 0x0055 | 85 | 0x0011 | 17 | 17 | 1.7 | constant |  |
| 0x0056 | 86 | 0x001e | 30 | 30 | 3 | constant |  |
| 0x0057 | 87 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x0058 | 88 | 0x0058 | 88 | 88 | 8.8 | constant |  |
| 0x0059 | 89 | 0x0028 | 40 | 40 | 4 | constant |  |
| 0x005a | 90 | 0x0008 | 8 | 8 | 0.8 | constant |  |
| 0x005b | 91 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x005c | 92 | 0x0017 | 23 | 23 | 2.3 | constant |  |
| 0x005d | 93 | 0x0028 | 40 | 40 | 4 | constant |  |
| 0x005e | 94 | 0x002c | 44 | 44 | 4.4 | constant |  |
| 0x005f | 95 | 0x0030 | 48 | 48 | 4.8 | constant |  |
| 0x0060 | 96 | 0x0036 | 54 | 54 | 5.4 | constant |  |
| 0x0061 | 97 | 0x003a | 58 | 58 | 5.8 | constant |  |
| 0x0062 | 98 | 0x0040 | 64 | 64 | 6.4 | constant |  |
| 0x0063 | 99 | 0x0048 | 72 | 72 | 7.2 | constant |  |
| 0x0064 | 100 | 0x0050 | 80 | 80 | 8 | constant |  |
| 0x0065 | 101 | 0x0054 | 84 | 84 | 8.4 | constant |  |
| 0x0066 | 102 | 0x005a | 90 | 90 | 9 | constant |  |
| 0x0067 | 103 | 0x005f | 95 | 95 | 9.5 | constant |  |
| 0x0068 | 104 | 0x0064 | 100 | 100 | 10 | constant |  |
| 0x0069 | 105 | 0x0069 | 105 | 105 | 10.5 | constant |  |
| 0x006a | 106 | 0x006e | 110 | 110 | 11 | constant |  |
| 0x006b | 107 | 0x0073 | 115 | 115 | 11.5 | constant |  |
| 0x006c | 108 | 0x000c | 12 | 12 | 1.2 | constant |  |
| 0x006d | 109 | 0x000d | 13 | 13 | 1.3 | constant |  |
| 0x006e | 110 | 0x000e | 14 | 14 | 1.4 | constant |  |
| 0x006f | 111 | 0x002e | 46 | 46 | 4.6 | constant |  |
| 0x0070 | 112 | 0x0034 | 52 | 52 | 5.2 | constant |  |
| 0x0071 | 113 | 0x003a | 58 | 58 | 5.8 | constant |  |
| 0x0072 | 114 | 0x0040 | 64 | 64 | 6.4 | constant |  |
| 0x0073 | 115 | 0x0048 | 72 | 72 | 7.2 | constant |  |
| 0x0074 | 116 | 0x0055 | 85 | 85 | 8.5 | constant |  |
| 0x0075 | 117 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0076 | 118 | 0x0001 | 1 | 1 | 0.1 | constant |  |
| 0x0077 | 119 | 0x000c | 12 | 12 | 1.2 | constant |  |
| 0x0078 | 120 | 0xffff | 65535 | -1 | -0.1 | constant | 0xffff |
| 0x0079 | 121 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x007a | 122 | 0x0008 | 8 | 8 | 0.8 | constant |  |
| 0x007b | 123 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x007c | 124 | 0x000c | 12 | 12 | 1.2 | constant |  |
| 0x007d | 125 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x007e | 126 | 0x000e | 14 | 14 | 1.4 | constant |  |
| 0x007f | 127 | 0x0000 | 0 | 0 | 0 | constant |  |
| 0x0080 | 128 | 0x0011 | 17 | 17 | 1.7 | constant |  |
| 0x0081 | 129 | 0x0000 | 0 | 0 | 0 | constant |  |
