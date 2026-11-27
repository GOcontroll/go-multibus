#!/usr/bin/env python3
"""Send bytes over the isoSPI link of a Multibus Module and read what comes back.

isoSPI is connector interface 5. The controller reaches it through the module's
protocol port: /dev/mb_protocol on the first module, /dev/mb_protocol2 on the
second. The go-multibus service keeps that port open, and two programs on one
port read each other's answers, so stop the service first:

    systemctl stop go-multibus
    python3 isospi_transfer.py [device] [hex bytes ...]
    systemctl start go-multibus

SPI is full duplex: while the module clocks your bytes out, it clocks the same
number of bytes in, and those are the answer. The module has to be the isoSPI
master, the role a module starts in; a slave only answers a master. Up to 64
bytes per transfer. Python 3, no extra packages.

Example, four bytes out:  python3 isospi_transfer.py /dev/mb_protocol 01 02 03 04
"""
import os
import select
import sys
import termios
import time

DEVICE = sys.argv[1] if len(sys.argv) > 1 else "/dev/mb_protocol"
SEND = bytes.fromhex("".join(sys.argv[2:])) if len(sys.argv) > 2 else bytes([0x01, 0x02, 0x03, 0x04])

CMD_INFO = 0x01          # identity, role and capabilities
CMD_ISOSPI_XFER = 0x30   # raw isoSPI transfer, master only


def crc16_xmodem(data):
    crc = 0
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else crc << 1
            crc &= 0xFFFF
    return crc


def request(fd, seq, cmd, payload=b"", timeout=1.0):
    """Send one frame and return the answer payload: status byte first."""
    body = bytes([seq, cmd]) + payload
    frame = bytes([0x7E]) + len(body).to_bytes(2, "little") + body
    os.write(fd, frame + crc16_xmodem(body).to_bytes(2, "little"))

    buf = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if select.select([fd], [], [], 0.05)[0]:
            buf += os.read(fd, 512)
        # Frame: 7E, length (2 bytes), seq, cmd | 0x80, payload, CRC-16/XMODEM.
        while (start := buf.find(b"\x7e")) >= 0 and len(buf) - start >= 5:
            length = int.from_bytes(buf[start + 1:start + 3], "little")
            end = start + 3 + length + 2
            if len(buf) < end:
                break
            body = buf[start + 3:end - 2]
            good = crc16_xmodem(body) == int.from_bytes(buf[end - 2:end], "little")
            buf = buf[end:] if good else buf[start + 1:]
            if good and body[0] == seq and body[1] == cmd | 0x80:
                return body[2:]
    raise TimeoutError(f"no answer to command {cmd:#04x}")


fd = os.open(DEVICE, os.O_RDWR | os.O_NOCTTY)
attrs = termios.tcgetattr(fd)
attrs[0] = attrs[1] = attrs[3] = 0                       # raw: no translation, no echo
attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
termios.tcsetattr(fd, termios.TCSANOW, attrs)
termios.tcflush(fd, termios.TCIOFLUSH)

try:
    info = request(fd, 1, CMD_INFO)
    role = "master" if info[2] == 0 else "slave"
    print(f"{DEVICE}: module firmware {info[7]}.{info[8]}.{info[9]}, isoSPI {role}")

    answer = request(fd, 2, CMD_ISOSPI_XFER, SEND)
    if answer[0] != 0:
        sys.exit(f"the module refused the transfer, status {answer[0]:#04x} (4 = not a master)")
    print("sent    :", SEND.hex(" "))
    print("received:", answer[1:].hex(" "))
finally:
    os.close(fd)
