#!/usr/bin/env python3
"""Send and receive on the RS232 port of a Multibus Module.

RS232 is connector interface 2 (pins 12/13): /dev/mb_serial1 on the first
module, /dev/mb_serial4 on the second. Set that interface to RS232 first, in the
go-multibus UI or with `go-multibus set 2 rs232`.

    python3 rs232_send_receive.py [device] [baud]

Sends one line, then prints whatever arrives until Ctrl+C. RS232 is full
duplex, so the other side may answer at any time. The baud rate is set like on
any serial port; the module applies it to its UART. Python 3, no extra packages.
"""
import os
import select
import sys
import termios

DEVICE = sys.argv[1] if len(sys.argv) > 1 else "/dev/mb_serial1"
BAUD = int(sys.argv[2]) if len(sys.argv) > 2 else 115200


def open_port(device, baud):
    """Open a serial port raw, 8N1, at `baud`."""
    fd = os.open(device, os.O_RDWR | os.O_NOCTTY)
    attrs = termios.tcgetattr(fd)
    speed = getattr(termios, f"B{baud}")
    attrs[0] = 0                                             # iflag: no translation
    attrs[1] = 0                                             # oflag: send bytes as they are
    attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL  # cflag: 8 data bits, no parity, 1 stop bit
    attrs[3] = 0                                             # lflag: no echo, no line editing
    attrs[4] = attrs[5] = speed
    attrs[6][termios.VMIN] = 0
    attrs[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, attrs)
    termios.tcflush(fd, termios.TCIOFLUSH)
    return fd


fd = open_port(DEVICE, BAUD)
print(f"{DEVICE} at {BAUD} baud")

os.write(fd, b"Hello from the Multibus Module\r\n")
print("sent one line; waiting for data (Ctrl+C to stop)")

try:
    while True:
        ready, _, _ = select.select([fd], [], [], 1.0)
        if ready:
            data = os.read(fd, 256)
            print("received:", data.decode("ascii", "replace").rstrip(), "|", data.hex(" "))
except KeyboardInterrupt:
    pass
finally:
    os.close(fd)
