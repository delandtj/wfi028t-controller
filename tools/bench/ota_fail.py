#!/usr/bin/env python3
"""OTA failure cases against a bench board (ADR 0002, bench section).

Sends deliberately broken pushes to the OTA port 4002 and reads `status` back
from the console port 4001, so the receiver's refusals can be checked without
touching fw-ota. Every case signs with the real key, the same way fw-ota does,
unless the case is about the signature.

  status      print the status line
  badsig      one signature byte flipped        -> err bad signature
  wrongkey    signed with a throwaway key       -> err bad signature
  target      signed header, other target       -> err image is for another target
  version     signed header, version 2          -> err unsupported header version
  toobig      length above the 4 MB slot        -> err image does not fit the app slot
  badsha      good header, one body byte off    -> err sha mismatch, no reboot
  truncated   half the image, then close        -> discarded, no reboot
  stall       half the image, then wait         -> dropped after the chunk timeout
  powercut    half the image, then a chip reset through the USB-JTAG port
              (RTS) -> the previous slot boots; prints the boot log
  good        a correct push, for comparison

After each case check `status`: same uptime (no reboot) and ota=valid, except
for `good` and `powercut`. The never-confirming image is not a case here: set
`mqtt host 192.0.2.1` on the console and push with `fw-ota push --wait`; the
image cannot reach the broker, stays pending and is rolled back after 120 s.
Clear it afterwards with `mqtt off`.

Only for a bench board: the image goes in with the production key.

Requires pyserial (powercut) and cryptography.
"""

import argparse
import hashlib
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

OTA_PORT = 4002
CONSOLE_PORT = 4001
KEY_PATH = os.path.expanduser("~/.config/wfi028t/ota-signing.key")
TARGET = b"wfi028t-c6"
CHUNK = 4096
# Matches fw/src/ota/header.rs: 76 signed bytes, then a 64-byte signature.
SIGNED_LEN = 76


def load_key() -> Ed25519PrivateKey:
    seed = bytes.fromhex(open(KEY_PATH).read().strip())
    return Ed25519PrivateKey.from_private_bytes(seed)


def header(image: bytes, key, target=TARGET, length=None, version=1) -> bytes:
    signed = (
        b"WOTA" + bytes([version, 0, 0, 0])
        + struct.pack("<I", len(image) if length is None else length)
        + hashlib.sha256(image).digest()
        + target.ljust(16, b"\0")
        + b"0.1.0-bench".ljust(16, b"\0")
    )
    assert len(signed) == SIGNED_LEN
    return signed + key.sign(signed)


def build_image(elf: str) -> bytes:
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "fw.bin")
        subprocess.run(["espflash", "save-image", "--chip", "esp32c6", elf, out],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return open(out, "rb").read()


def console(host: str, cmd: str = "status", wait: float = 1.5) -> str:
    s = socket.create_connection((host, CONSOLE_PORT), timeout=5)
    s.sendall(cmd.encode() + b"\n")
    time.sleep(wait)
    s.settimeout(0.5)
    out = b""
    try:
        while chunk := s.recv(65536):
            out += chunk
    except socket.timeout:
        pass
    s.close()
    return out.decode(errors="replace")


def status_fields(host: str, *names: str) -> str:
    for line in console(host).splitlines():
        if line.startswith("# status"):
            fields = dict(kv.split("=", 1) for kv in line.split()[2:] if "=" in kv)
            return " ".join(f"{n}={fields.get(n, '?')}" for n in names)
    return "no status line"


def push(host, hdr, body, stop_after=None, hold=0.0, close_early=False):
    s = socket.create_connection((host, OTA_PORT), timeout=10)
    s.sendall(hdr)
    sent = 0
    try:
        while sent < len(body) and (stop_after is None or sent < stop_after):
            s.sendall(body[sent:sent + CHUNK])
            sent += CHUNK
    except OSError as err:
        print(f"  send stopped after {sent} bytes: {err}")
    if hold:
        time.sleep(hold)
    if close_early:
        s.close()
        return sent, "(closed by us)"
    s.settimeout(15)
    out = b""
    try:
        while chunk := s.recv(4096):
            out += chunk
    except OSError as err:
        out += f" <{type(err).__name__}>".encode()
    s.close()
    return sent, out.decode(errors="replace").strip()


def powercut(host, hdr, body, port):
    import serial

    s = socket.create_connection((host, OTA_PORT), timeout=10)
    s.sendall(hdr)
    sent = 0
    while sent < len(body) // 2:
        s.sendall(body[sent:sent + CHUNK])
        sent += CHUNK
    usb = serial.Serial(port, 115200, timeout=0.2)
    usb.dtr = False
    usb.rts = True
    time.sleep(0.1)
    usb.rts = False
    print(f"  chip reset after {sent} bytes; boot log:")
    boot = b""
    end = time.time() + 12
    while time.time() < end:
        boot += usb.read(4096)
    for line in boot.decode(errors="replace").splitlines():
        if "Loaded app" in line or line.startswith("rst:") or "wifi" in line:
            print("   ", line.strip())
    s.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("case")
    ap.add_argument("--host", required=True, help="the bench board's IP")
    target_dir = os.environ.get("CARGO_TARGET_DIR",
                                os.path.join(os.path.dirname(__file__), "..", "..", "target"))
    ap.add_argument("--elf", default=os.path.join(
        target_dir, "riscv32imac-unknown-none-elf", "release", "wfi-controller-fw"),
        help="release ELF (default: $CARGO_TARGET_DIR or ./target)")
    ap.add_argument("--serial", default="/dev/ttyACM0", help="USB-JTAG port, for powercut")
    ap.add_argument("--hold", type=float, default=90.0, help="seconds to stall, for stall")
    args = ap.parse_args()

    if args.case == "status":
        print(console(args.host))
        return

    key = load_key()
    image = build_image(args.elf)
    half = len(image) // 2
    case = args.case
    if case == "badsig":
        hdr = bytearray(header(image, key))
        hdr[SIGNED_LEN + 24] ^= 1
        result = push(args.host, bytes(hdr), image)
    elif case == "wrongkey":
        result = push(args.host, header(image, Ed25519PrivateKey.generate()), image)
    elif case == "target":
        result = push(args.host, header(image, key, target=b"other-board"), image)
    elif case == "version":
        result = push(args.host, header(image, key, version=2), image)
    elif case == "toobig":
        result = push(args.host, header(image, key, length=0x400001), image)
    elif case == "badsha":
        body = bytearray(image)
        body[half] ^= 0xFF
        result = push(args.host, header(image, key), bytes(body))
    elif case == "truncated":
        result = push(args.host, header(image, key), image, stop_after=half, close_early=True)
    elif case == "stall":
        result = push(args.host, header(image, key), image, stop_after=half, hold=args.hold)
    elif case == "good":
        result = push(args.host, header(image, key), image)
    elif case == "powercut":
        powercut(args.host, header(image, key), image, args.serial)
        time.sleep(8)
        result = None
    else:
        sys.exit(f"unknown case {case}")

    if result:
        sent, reply = result
        print(f"  sent {sent} bytes; device said:")
        for line in reply.splitlines():
            if not line.startswith("progress"):
                print("   ", line)
    if case not in ("good", "powercut"):
        time.sleep(2)
    print("  " + status_fields(args.host, "uptime_ms", "ota"))


if __name__ == "__main__":
    main()
