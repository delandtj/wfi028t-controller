#!/usr/bin/env python3
"""Bench test for replacing the WFI-028T/035T wired controller.

Acts as the Modbus master on the heat pump bus through an RS-485 adapter
(FTDI), imitating the stock controller, to answer the open questions in
docs/adr/0001-rust-replacement-controller.md before any firmware is written:

  poll      imitate the stock controller's 1 s poll and print decoded values
  set       change one allowlisted setting (whole-block, single 0x06, or
            0x10 with count 1), keep polling, verify the readback
  silence   poll, go quiet for N seconds, resume, report what changed

Safety:
  - The stock controller MUST be unplugged first. Every command listens for
    3 s before transmitting and aborts if anyone else is talking.
  - Only allowlisted fields can be written, within the manual's ranges.
  - A write always starts from a settings block read moments before
    (read-modify-write), like the stock controller does.

Register facts come from docs/register-map.md. Every frame sent and received
is printed with a timestamp; the sniffer keeps logging the bus as well.

Requires pyserial.
"""

import argparse
import subprocess
import sys
import time

import serial

SLAVE = 0x01
STATUS = (0x0000, 63)
SETTINGS = (0x003F, 67)
RESPONSE_TIMEOUT = 1.0      # s; the heat pump answers in ~150 ms
INTER_BYTE_GAP = 0.010      # s; > 3.5 chars at 9600 baud (4 ms)
CYCLE = 1.0                 # s; stock controller poll period
WRITE_REPEAT = 3            # the stock controller sends each write 3 times
WRITE_SPACING = 0.5         # s between repeats

# Writable fields: (register, kind, arg, range/values, description)
# kind "bit": arg = bit number; "bitinv": inverted bit; "word": whole register
FIELDS = {
    "power": (0x003F, "bit", 0, (0, 1), "0x003f bit 0: 1 = on"),
    "p05":   (0x003F, "bit", 4, (0, 1), "0x003f bit 4: 1 = stop at target"),
    "boost": (0x003F, "bitinv", 6, (0, 1), "0x003f bit 6 inverted: 1 = full power"),
    "mode":  (0x0040, "word", None, (1, 2, 7), "0x0040: 1 heat, 2 cool, 7 auto"),
    "p01":   (0x0041, "word", None, range(8, 41), "0x0041: heating setpoint C"),
    "p02":   (0x0042, "word", None, range(8, 29), "0x0042: cooling setpoint C"),
    "p03":   (0x004A, "word", None, range(8, 41), "0x004a: auto setpoint C"),
    "p04":   (0x004D, "word", None, range(1, 19), "0x004d: restart hysteresis C"),
}


def crc16(data: bytes) -> int:
    crc = 0xFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0xA001 if crc & 1 else crc >> 1
    return crc


def frame(pdu: bytes) -> bytes:
    body = bytes([SLAVE]) + pdu
    c = crc16(body)
    return body + bytes([c & 0xFF, c >> 8])


def ts() -> str:
    t = time.time()
    return time.strftime("%H:%M:%S", time.localtime(t)) + f".{int(t * 1000) % 1000:03d}"


class Bus:
    def __init__(self, port: str, verbose: bool):
        self.s = serial.Serial(port, 9600, bytesize=8, parity="N", stopbits=1,
                               timeout=0, inter_byte_timeout=None)
        self.verbose = verbose
        self.timeouts = 0
        self.last_settings = None      # (time, list of 67 values)
        self.last_status = None

    def log(self, direction: str, data: bytes):
        if self.verbose:
            print(f"{ts()} {direction} {data.hex(' ')}")

    def listen(self, seconds: float) -> bytes:
        self.s.reset_input_buffer()
        end = time.time() + seconds
        got = b""
        while time.time() < end:
            got += self.s.read(256)
            time.sleep(0.01)
        return got

    def _read_frame(self) -> bytes:
        deadline = time.time() + RESPONSE_TIMEOUT
        buf = b""
        last = None
        while True:
            chunk = self.s.read(256)
            now = time.time()
            if chunk:
                buf += chunk
                last = now
            elif buf and now - last > INTER_BYTE_GAP:
                return buf
            elif not buf and now > deadline:
                return b""
            time.sleep(0.001)

    def transact(self, pdu: bytes) -> bytes:
        req = frame(pdu)
        self.s.reset_input_buffer()
        self.s.write(req)
        self.s.flush()
        self.log("TX", req)
        resp = self._read_frame()
        if resp[:len(req)] == req and len(resp) > len(req):
            resp = resp[len(req):]          # adapter echoed our own frame
        if not resp:
            self.timeouts += 1
            print(f"{ts()} TIMEOUT for {req.hex(' ')}")
            return b""
        self.log("RX", resp)
        if len(resp) < 4 or crc16(resp[:-2]) != (resp[-2] | resp[-1] << 8):
            print(f"{ts()} BAD CRC {resp.hex(' ')}")
            return b""
        if resp[0] != SLAVE:
            print(f"{ts()} wrong slave {resp[0]:#04x}")
            return b""
        if resp[1] & 0x80:
            print(f"{ts()} EXCEPTION func={resp[1]:#04x} code={resp[2]:#04x}")
            return b""
        return resp

    def read(self, start: int, count: int):
        pdu = bytes([0x03, start >> 8, start & 0xFF, count >> 8, count & 0xFF])
        r = self.transact(pdu)
        if not r or r[1] != 0x03 or r[2] != 2 * count:
            return None
        vals = [r[3 + 2 * i] << 8 | r[4 + 2 * i] for i in range(count)]
        if (start, count) == SETTINGS:
            self.last_settings = (time.time(), vals)
        elif (start, count) == STATUS:
            self.last_status = vals
        return vals

    def write_block(self, start: int, vals) -> bool:
        n = len(vals)
        data = b"".join(bytes([v >> 8, v & 0xFF]) for v in vals)
        pdu = bytes([0x10, start >> 8, start & 0xFF, n >> 8, n & 0xFF, 2 * n]) + data
        r = self.transact(pdu)
        return bool(r) and r[1] == 0x10 and r[2:6] == pdu[1:5]

    def write_single(self, reg: int, val: int) -> bool:
        pdu = bytes([0x06, reg >> 8, reg & 0xFF, val >> 8, val & 0xFF])
        r = self.transact(pdu)
        return bool(r) and r[1:6] == pdu

    def cycle(self):
        """One stock-controller cycle: status, half a second, settings."""
        t0 = time.time()
        self.read(*STATUS)
        time.sleep(max(0.0, t0 + CYCLE / 2 - time.time()))
        self.read(*SETTINGS)
        time.sleep(max(0.0, t0 + CYCLE - time.time()))


def summary(bus: Bus) -> str:
    st, se = bus.last_status, bus.last_settings
    parts = []
    if st:
        parts.append(f"in {st[0x0F] / 10:.1f} out {st[0x10] / 2:.1f} amb {st[0x11] / 2:.1f} "
                     f"comp {st[0x1B]}Hz {st[0x20]}A fan {st[0x24]} "
                     f"s04={st[0x04]:#06x} s05={st[0x05]:#06x}")
    if se:
        v = se[1]
        r3f = v[0]
        parts.append(f"on={r3f & 1} p05={r3f >> 4 & 1} boost={0 if r3f >> 6 & 1 else 1} "
                     f"mode={v[1]} p01={v[2]} p02={v[3]} p03={v[0x4A - 0x3F]} p04={v[0x4D - 0x3F]}")
    return " | ".join(parts) if parts else "(no data)"


def note(text: str, cmd: str):
    if not cmd:
        return
    try:
        subprocess.run(cmd.split() + [text], timeout=5, capture_output=True)
    except Exception as e:  # best effort: the bench must not depend on it
        print(f"(note not recorded: {e})")


def preflight(bus: Bus):
    print(f"{ts()} listening 3 s for another master ...")
    got = bus.listen(3.0)
    if got:
        sys.exit(f"ABORT: {len(got)} bytes seen on the bus - unplug the stock controller first")
    print(f"{ts()} bus quiet, taking over as master")


def field_get(vals, field):
    reg, kind, arg, _, _ = FIELDS[field]
    v = vals[reg - SETTINGS[0]]
    if kind == "bit":
        return v >> arg & 1
    if kind == "bitinv":
        return 0 if v >> arg & 1 else 1
    return v


def field_set(regval, field, value):
    _, kind, arg, _, _ = FIELDS[field]
    if kind == "bit":
        return regval | (1 << arg) if value else regval & ~(1 << arg)
    if kind == "bitinv":
        return regval & ~(1 << arg) if value else regval | (1 << arg)
    return value


def cmd_poll(bus: Bus, args):
    end = time.time() + args.seconds if args.seconds else None
    while end is None or time.time() < end:
        bus.cycle()
        print(f"{ts()} {summary(bus)}  timeouts={bus.timeouts}")


def cmd_set(bus: Bus, args):
    field, value = args.field, args.value
    reg, _, _, allowed, desc = FIELDS[field]
    if value not in allowed:
        sys.exit(f"{field}={value} outside allowed {list(allowed)} ({desc})")
    for _ in range(3):
        bus.cycle()
    if not bus.last_settings or time.time() - bus.last_settings[0] > 2.0:
        sys.exit("no fresh settings block, refusing to write")
    vals = list(bus.last_settings[1])
    old = field_get(vals, field)
    idx = reg - SETTINGS[0]
    newreg = field_set(vals[idx], field, value)
    print(f"{ts()} {field}: {old} -> {value} (reg {reg:#06x}: {vals[idx]:#06x} -> {newreg:#06x}) "
          f"method={args.method}")
    note(f"BENCH set {field} {old}->{value} method={args.method}", args.note_cmd)
    vals[idx] = newreg
    for i in range(WRITE_REPEAT if args.method == "block" else 1):
        if args.method == "block":
            ok = bus.write_block(SETTINGS[0], vals)
        elif args.method == "multi1":
            ok = bus.write_block(reg, [newreg])
        else:
            ok = bus.write_single(reg, newreg)
        print(f"{ts()} write {i + 1}: {'acked' if ok else 'NOT acked'}")
        time.sleep(WRITE_SPACING)
    for _ in range(5):
        bus.cycle()
        print(f"{ts()} {summary(bus)}")
    got = field_get(bus.last_settings[1], field) if bus.last_settings else None
    verdict = "OK" if got == value else "FAILED"
    print(f"{ts()} readback {field}={got} -> {verdict}")
    note(f"BENCH set {field} readback {got} {verdict}", args.note_cmd)


def cmd_silence(bus: Bus, args):
    for _ in range(5):
        bus.cycle()
    before = list(bus.last_status or [])
    print(f"{ts()} before: {summary(bus)}")
    note(f"BENCH silence {args.seconds}s start", args.note_cmd)
    print(f"{ts()} going silent for {args.seconds} s")
    time.sleep(args.seconds)
    note("BENCH silence end, resuming polls", args.note_cmd)
    for i in range(args.resume):
        bus.cycle()
        print(f"{ts()} {summary(bus)}  timeouts={bus.timeouts}")
    after = bus.last_status or []
    diffs = [f"{i:#06x} {a:#06x}->{b:#06x}" for i, (a, b) in enumerate(zip(before, after))
             if a != b and i < 0x0F]
    print(f"{ts()} status flag/error words changed: {', '.join(diffs) or 'none'}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", default="/dev/ttyUSB0")
    ap.add_argument("-v", "--verbose", action="store_true", help="print every frame")
    ap.add_argument("--note-cmd", default="sniffer-capture note",
                    help="command used to drop notes into the capture log ('' to disable)")
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("poll", help="imitate the stock controller's poll loop")
    p.add_argument("--seconds", type=float, default=0, help="0 = until Ctrl-C")
    p = sub.add_parser("set", help="change one allowlisted setting")
    p.add_argument("field", choices=sorted(FIELDS))
    p.add_argument("value", type=int)
    p.add_argument("--method", choices=("block", "single", "multi1"), default="block",
                   help="block = whole settings block x3 like the stock controller; "
                        "single = function 0x06; multi1 = function 0x10 with one register")
    p = sub.add_parser("silence", help="stop polling for a while and see what the heat pump does")
    p.add_argument("seconds", type=float)
    p.add_argument("--resume", type=int, default=15, help="poll cycles after the silence")
    args = ap.parse_args()

    bus = Bus(args.port, args.verbose)
    preflight(bus)
    try:
        {"poll": cmd_poll, "set": cmd_set, "silence": cmd_silence}[args.cmd](bus, args)
    except KeyboardInterrupt:
        print("\nstopped")


if __name__ == "__main__":
    main()
