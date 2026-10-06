# Zhike three-phase inverter pool heat pump - mainboard Modbus protocol

Plain-text transcription of
[`modbus-protocol-zhike-inverter-pool-heat-pump-2022-03-29-en.pdf`](modbus-protocol-zhike-inverter-pool-heat-pump-2022-03-29-en.pdf):
"Zhike three-phase inverter swimming pool heat pump - mainboard communication
protocol (2022-03-29)", English translation of document RCDF211268. Content
as in the PDF, nothing corrected; where the bus disagrees, see
[`../register-map.md`](../register-map.md) ("Vendor document vs capture").

R = read-only parameter, RW = read/write parameter.

## 1. Communication rules

1. RS-485 bus, asynchronous serial: 1 start bit, 8 data bits, 1 stop bit, no
   parity, 9600 bps.
2. Standard Modbus RTU, 16-bit data, 16-bit CRC sent low byte first, high byte
   last.
3. Unit slave address 1 to 16, set by DIP switches 1-4.
4. The upper computer is the calling master; the controller is the slave.
5. Three commands:

**03H, read one or more registers**

- Send: address, 03H, start register high, low, register count high, low,
  CRC low, CRC high.
- Response: address, 03H, byte count, data 1 ... data n, CRC low, CRC high.

**06H, modify a single register**

- Send: address, 06H, register high, low, data high, low, CRC low, CRC high.
- Response: the command is returned unchanged if successful; otherwise no
  response.

**10H, modify multiple registers**

- Send: address, 10H, start register high, low, register count high, low,
  byte count, data 1 high, low ... data N high, low, CRC low, CRC high.
- Response: address, 10H, start register high, low, register count high, low,
  CRC low, CRC high.

## 2. Parameter address table

### Read-only block, 0x0000-0x003e

| Address | Description | Range |
|---|---|---|
| 0x0000, 0x0001 | reserved | |
| 0x0002 | control switch status (see flags) | |
| 0x0003 | operating status flags (see flags) | |
| 0x0004 | output status flags 1 (see flags) | |
| 0x0005 | output status flags 2 (see flags) | |
| 0x0006 | output status flags 3 (see flags) | |
| 0x0007-0x000d | fault flags 1-7 (see flags) | |
| 0x000e | reserved | |
| 0x000f | inlet water temperature | -30..99 C |
| 0x0010 | outlet water temperature | -30..99 C |
| 0x0011 | ambient temperature | -30..99 C |
| 0x0012 | coil 1 temperature | -30..99 C |
| 0x0013 | suction gas temperature 1 | -30..99 C |
| 0x0014 | cooling coil 1 temperature | -30..99 C |
| 0x0015 | discharge temperature 1 | 0..125 C |
| 0x0016, 0x0017 | reserved | |
| 0x0018 | main EXV opening 1 | |
| 0x0019 | cooling EXV opening 1 | |
| 0x001a | compressor 1 target frequency | |
| 0x001b | compressor 1 actual frequency | |
| 0x001c | inverter module 1 fault code 1 | |
| 0x001d | inverter module 1 fault code 2 | |
| 0x001e | DC bus voltage 1 | |
| 0x001f-0x0023 | reserved | |
| 0x0024 | DC fan 1 speed | |
| 0x0025 | DC fan 2 speed | |
| 0x0026 | coil 2 temperature | -30..99 C |
| 0x0027 | suction gas temperature 2 | -30..99 C |
| 0x0028 | cooling coil 2 temperature | -30..99 C |
| 0x0029 | discharge temperature 2 | 0..125 C |
| 0x002a, 0x002b | reserved | |
| 0x002c | main EXV opening 2 | |
| 0x002d | cooling EXV opening 2 | |
| 0x002e | compressor 2 target frequency | |
| 0x002f | compressor 2 actual frequency | |
| 0x0030 | inverter module 2 fault code 1 | |
| 0x0031 | inverter module 2 fault code 2 | |
| 0x0032 | DC bus voltage 2 | |
| 0x0033-0x003e | reserved | |

### Read/write block, 0x003f-0x0082

| Address | Description | Range | Default |
|---|---|---|---|
| 0x003f | parameter flag definition (see flags) | | |
| 0x0040 | mode (see flags) | | |
| 0x0041 | heating temperature setpoint | 8..40 C | 27 C |
| 0x0042 | cooling temperature setpoint | 8..28 C | 27 C |
| 0x0043 | manual frequency setting | | |
| 0x0044 | manual EXV step position | 20..450 | 300 |
| 0x0045 | manual auxiliary-valve step position | | |
| 0x0046 | manual frequency setting 2 | | |
| 0x0047 | manual EXV 2 step position | | |
| 0x0048 | manual auxiliary-valve 2 step position | | |
| 0x0049 | manual fan speed | | |
| 0x004a | auto-mode temperature setpoint | | |
| 0x004b | reserved | | |
| 0x004c | mode changeover time | 3..30 min | 10 min |
| 0x004d | temperature hysteresis (differential) | 1..18 C | 1 C |
| 0x004e | inlet water temperature compensation | | |
| 0x004f | reserved | | |
| 0x0050 | defrost interval | 20..90 min | 40 min |
| 0x0051 | defrost start temperature | -15..-1 C | -6 C |
| 0x0052 | defrost duration | 5..20 min | 11 min |
| 0x0053 | defrost termination temperature | 1..40 C | 16 C |
| 0x0054 | defrost ambient-to-coil temperature difference | 0..15 C | 6 C |
| 0x0055 | defrost ambient temperature threshold | 0..20 C | 17 C |
| 0x0056 | EXV adjustment period | 20..90 s | 30 s |
| 0x0057 | heating target superheat | -5..10 C | 1 C |
| 0x0058 | EXV discharge-temperature target | 70..125 C | 88 C |
| 0x0059 | EXV opening during defrost | 20..450 | 400 |
| 0x005a | minimum EXV opening | 50..150 | 80 |
| 0x005b | cooling target superheat | -5..10 C | 1 C |
| 0x005c | reserved | | |
| 0x005d-0x0066 | frequency settings F1-F10 | 30..90 Hz | 40, 44, 48, 54, 58, 64, 72, 80, 84, 90 Hz |
| 0x0067-0x006b | discharge temperature settings TP0-TP4 | 50..125 C | 95, 100, 105, 110, 115 C |
| 0x006c-0x006e | reserved | | |
| 0x006f-0x0074 | fan speed levels 1-6 | 20..100 | 46, 52, 58, 64, 72, 85 |
| 0x0075 | fan speed level selection | 0..6 | 0 |
| 0x0076 | fan type: 0 = AC, 1 = DC, 2 = EC | 0..2 | 1 |
| 0x0077, 0x0078 | reserved | | |
| 0x0079 | timer enable flag | | |
| 0x007a | timer 1 ON hour | 0..23 | |
| 0x007b | timer 1 ON minute | 0..59 | |
| 0x007c | timer 1 OFF hour | 0..23 | |
| 0x007d | timer 1 OFF minute | 0..59 | |
| 0x007e | timer 2 ON hour | 0..23 | |
| 0x007f | timer 2 ON minute | 0..59 | |
| 0x0080 | timer 2 OFF hour | 0..23 | |
| 0x0081 | timer 2 OFF minute | 0..59 | |
| 0x0082 | (no description; "see flag description") | | |

## 3. Flag descriptions

Bits not listed are reserved.

| Register | Bit | Meaning |
|---|---|---|
| control switch port (printed under 0x0082) | 2 | high pressure switch |
| | 4 | low pressure switch |
| | 5 | low pressure switch 2 |
| | 6 | water flow switch |
| | 7 | high pressure switch 2 |
| 0x0003 work status | 0 | hot water available |
| | 1 | high temperature |
| | 2 | heating |
| | 3 | cooling |
| | 5 | high water level |
| | 6 | low water level |
| | 7 | defrost |
| 0x0004 output flags 1 | 0 | compressor 1 |
| | 1 | compressor 2 |
| | 5 | fan |
| | 6 | four-way valve |
| | 7 | high / low fan speed |
| 0x0005 output flags 2 | 3 | make-up water valve |
| | 6 | air-conditioning cooling demand |
| | 7 | air-conditioning heating demand |
| 0x0006 output flags 3 | 1 | crankcase electric heater |
| | 2 | circulating water pump |
| 0x0007 fault flags 1 | 1 | ambient temperature fault |
| | 2 | coil 1 temperature fault |
| | 4 | outlet water temperature fault |
| | 5 | high pressure fault |
| | 6 | low pressure fault |
| 0x0008 fault flags 2 | 0 | water flow switch fault |
| | 2 | heating outlet water over-temperature protection |
| | 5 | coil 2 temperature fault |
| | 6 | high pressure switch 2 fault |
| | 7 | low pressure switch 2 fault |
| 0x0009 fault flags 3 | 5 | DC fan 2 fault |
| | 6 | exhaust 1 temperature fault |
| 0x000a fault flags 4 | 0 | inlet water temperature fault |
| | 1 | exhaust 1 over-temperature fault |
| | 5 | cooling outlet water over-cooling protection |
| | 6 | return gas 1 temperature fault |
| 0x000b fault flags 5 | 2 | coil 1 over-temperature protection |
| | 3 | cooling coil 1 temperature fault |
| | 5 | coil 2 over-temperature protection |
| | 6 | exhaust 2 over-temperature fault |
| 0x000c fault flags 6 | 4 | second-stage anti-freeze |
| | 5 | first-stage anti-freeze |
| 0x000d fault flags 7 | 0 | cooling coil 2 temperature fault |
| | 2 | inverter module 2 communication fault |
| | 3 | return gas 2 temperature fault |
| | 4 | inverter module 1 communication fault |
| | 5 | exhaust 2 temperature fault |
| | 6 | DC fan 1 fault |
| 0x003f parameter flags | 0 | wire controller ON/OFF |
| | 1 | manual frequency |
| | 2 | expansion valve mode: 0 = manual, 1 = automatic |
| | 4 | water pump mode: 0 = continuous, 1 = periodic |
| | 5 | cooling expansion valve mode: 0 = ambient, 1 = superheat |
| | 6 | silent mode: 0 = off, 1 = on |
| | 13 | fan mode |
| | 14 | forced defrost |
| 0x0040 mode | value | 1 = heating, 2 = cooling, 7 = auto |
| | 4 | boost (powerful) mode: 0 = off, 1 = on |
