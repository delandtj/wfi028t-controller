#!/usr/bin/env python3
"""Heat pump emulator for bench-testing the controller firmware.

Plays the heat pump PCB (Modbus RTU slave 0x01, 9600 8N1) through an RS-485
adapter (FTDI), so the ESP32 can run in master mode without the real unit:

  - 0x03 read holding registers anywhere in 0x0000-0x0081 (status block
    0x0000-0x003e, settings block 0x003f-0x0081)
  - 0x10 write multiple and 0x06 write single, settings block only
  - anything else: exception 01 (function) or 02 (address)

The registers start from the captured snapshots in docs/register-map.md
(unit on, heating, ECO, P01 33, P02 27, P03 27, P04 1). Writes are applied, so
a readback shows the change; every write is printed as a register diff.

Wiring: FTDI A/B to the ESP's RS-485 module A/B, nothing else on the pair.
Never connect this to the real heat pump bus: it would answer as slave 0x01.

Requires pyserial.
"""

import argparse
import random
import time

import serial

from ctl_bench import SETTINGS, SLAVE, STATUS, crc16, frame, ts

FRAME_GAP = 0.004           # s; t3.5 at 9600 baud
RESPONSE_DELAY = 0.150      # s; what the real heat pump takes to answer

# Snapshots from docs/register-map.md, 2026-10-03 15:32 (also the hp-model
# test fixtures).
STATUS_BLOCK = [
    0x2020, 0x0404, 0x0000, 0x000D, 0x0021, 0x0080, 0x0014, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x7FFF, 0x0113,
    0x003B, 0x0028, 0x000C, 0x000E, 0x0041, 0x004C, 0x7FFF, 0x7FFF,
    0x0082, 0x7FFF, 0x0037, 0x0036, 0x0000, 0x0000, 0x021D, 0x0050,
    0x0008, 0x7FFF, 0x7FFF, 0x7FFF, 0x02D2, 0x0000, 0x0000, 0x0000,
    0x0000, 0x0000, 0x7FFF, 0x7FFF, 0x0000, 0x7FFF, 0x0000, 0x0000,
    0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x7FFF, 0x7FFF, 0x7FFF,
    0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF, 0x77FF,
]
SETTINGS_BLOCK = [
    0x1071, 0x0001, 0x0021, 0x001B, 0x0032, 0x0096, 0xFFFF, 0xFFFF,
    0x7FFF, 0xFFFF, 0x01F4, 0x001B, 0x000A, 0xFFFF, 0x0001, 0x0000,
    0xFFEC, 0x0028, 0xFFFA, 0x000B, 0x0010, 0x0006, 0x0011, 0x001E,
    0x0001, 0x0058, 0x0028, 0x0008, 0x0001, 0x0017, 0x0028, 0x002C,
    0x0030, 0x0036, 0x003A, 0x0040, 0x0048, 0x0050, 0x0054, 0x005A,
    0x005F, 0x0064, 0x0069, 0x006E, 0x0073, 0x000C, 0x000D, 0x000E,
    0x002E, 0x0034, 0x003A, 0x0040, 0x0048, 0x0055, 0x0000, 0x0001,
    0x000C, 0xFFFF, 0x0000, 0x0008, 0x0000, 0x000C, 0x0000, 0x000E,
    0x0000, 0x0011, 0x0000,
]
assert len(STATUS_BLOCK) == STATUS[1] and len(SETTINGS_BLOCK) == SETTINGS[1]

REG_END = SETTINGS[0] + SETTINGS[1]     # one past the last register


def exception(func: int, code: int) -> bytes:
    return frame(bytes([func | 0x80, code]))


class HeatPump:
    def __init__(self, args):
        self.regs = STATUS_BLOCK + SETTINGS_BLOCK
        self.args = args
        self.reads = 0
        self.writes = 0

    def writable(self, start: int, count: int) -> bool:
        return count >= 1 and SETTINGS[0] <= start and start + count <= REG_END

    def store(self, start: int, vals):
        changed = [f"{start + i:#06x} {self.regs[start + i]:#06x}->{v:#06x}"
                   for i, v in enumerate(vals) if self.regs[start + i] != v]
        self.regs[start:start + len(vals)] = vals
        self.writes += 1
        print(f"{ts()} write {start:#06x} x{len(vals)}: {', '.join(changed) or 'no change'}")

    def handle(self, req: bytes):
        """The response frame for one request, or None to stay silent."""
        if len(req) < 4 or crc16(req[:-2]) != (req[-2] | req[-1] << 8):
            print(f"{ts()} BAD CRC {req.hex(' ')}")
            return None
        if req[0] != SLAVE:
            return None
        func, pdu = req[1], req[2:-2]
        if func == 0x03 and len(pdu) == 4:
            start, count = pdu[0] << 8 | pdu[1], pdu[2] << 8 | pdu[3]
            if not 1 <= count <= 125 or start + count > REG_END:
                return exception(func, 0x02)
            self.reads += 1
            data = b"".join(bytes([v >> 8, v & 0xFF]) for v in self.regs[start:start + count])
            return frame(bytes([func, 2 * count]) + data)
        if func == 0x10 and len(pdu) >= 5:
            start, count, nbytes = pdu[0] << 8 | pdu[1], pdu[2] << 8 | pdu[3], pdu[4]
            if nbytes != 2 * count or len(pdu) != 5 + nbytes:
                return exception(func, 0x03)
            if not self.writable(start, count) or (self.args.block_only and (start, count) != SETTINGS):
                return exception(func, 0x02)
            self.store(start, [pdu[5 + 2 * i] << 8 | pdu[6 + 2 * i] for i in range(count)])
            return frame(bytes([func]) + pdu[:4])
        if func == 0x06 and len(pdu) == 4:
            reg = pdu[0] << 8 | pdu[1]
            if self.args.block_only:
                return exception(func, 0x01)
            if not self.writable(reg, 1):
                return exception(func, 0x02)
            self.store(reg, [pdu[2] << 8 | pdu[3]])
            return frame(bytes([func]) + pdu)
        return exception(func, 0x01)

    def summary(self) -> str:
        s = self.regs[SETTINGS[0]:]
        return (f"on={s[0] & 1} p05={s[0] >> 4 & 1} boost={0 if s[0] >> 6 & 1 else 1} "
                f"mode={s[1]} p01={s[2]} p02={s[3]} p03={s[0x4A - 0x3F]} p04={s[0x4D - 0x3F]}")


def read_frame(s: serial.Serial) -> bytes:
    """Block until one frame has arrived (ended by a t3.5 gap)."""
    buf = b""
    last = 0.0
    while True:
        chunk = s.read(256)
        now = time.monotonic()
        if chunk:
            buf += chunk
            last = now
        elif buf and now - last > FRAME_GAP:
            return buf
        time.sleep(0.001)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", default="/dev/ttyUSB0")
    ap.add_argument("-v", "--verbose", action="store_true", help="print every frame")
    ap.add_argument("--delay", type=float, default=RESPONSE_DELAY, help="response delay in s")
    ap.add_argument("--drop", type=float, default=0.0,
                    help="probability of ignoring a request, to exercise timeouts")
    ap.add_argument("--block-only", action="store_true",
                    help="reject 0x06 and partial 0x10 writes (whole settings block only)")
    ap.add_argument("--summary", type=float, default=10.0, help="seconds between summaries, 0 = off")
    args = ap.parse_args()

    s = serial.Serial(args.port, 9600, bytesize=8, parity="N", stopbits=1, timeout=0)
    hp = HeatPump(args)
    print(f"{ts()} heat pump mock on {args.port}, slave {SLAVE:#04x}: {hp.summary()}")
    echo = b""
    next_summary = time.monotonic() + args.summary
    try:
        while True:
            req = read_frame(s)
            if echo and req.startswith(echo):
                req = req[len(echo):]   # adapter echoed our own response
            echo = b""
            if not req:
                continue
            if args.verbose:
                print(f"{ts()} RX {req.hex(' ')}")
            resp = hp.handle(req)
            if resp is None:
                continue
            if args.drop and random.random() < args.drop:
                print(f"{ts()} dropped {req.hex(' ')}")
                continue
            time.sleep(args.delay)
            s.write(resp)
            s.flush()
            echo = resp
            if args.verbose:
                print(f"{ts()} TX {resp.hex(' ')}")
            if args.summary and time.monotonic() >= next_summary:
                print(f"{ts()} {hp.summary()}  reads={hp.reads} writes={hp.writes}")
                next_summary = time.monotonic() + args.summary
    except KeyboardInterrupt:
        print(f"\nstopped: reads={hp.reads} writes={hp.writes}")


if __name__ == "__main__":
    main()
