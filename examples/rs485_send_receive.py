#!/usr/bin/env python3
"""Send a request on an RS485 port of a Multibus Module and wait for the answer.

The module has three RS485 ports, on connector interfaces 2, 3 and 4:
/dev/mb_serial1..3 on the first module, /dev/mb_serial4..6 on the second. The
interface has to be set to RS485, in the go-multibus UI or with for example
`go-multibus set 3 rs485`.

    python3 rs485_send_receive.py [device] [baud]

RS485 is half duplex: one side talks at a time. The module switches its
transceiver to transmit while it sends and back to receive straight after, so
the script just writes and reads, and does not hear its own request. It sends a
request once a second and prints the answer, until Ctrl+C. Python 3, no extra
packages.
"""
import os
import select
import sys
import termios
import time

DEVICE = sys.argv[1] if len(sys.argv) > 1 else "/dev/mb_serial2"
BAUD = int(sys.argv[2]) if len(sys.argv) > 2 else 115200
ANSWER_TIMEOUT = 0.5   # seconds to wait for the other side


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


def read_answer(fd, timeout):
    """Everything that arrives until the line has been quiet for `timeout`."""
    answer = b""
    while select.select([fd], [], [], timeout)[0]:
        answer += os.read(fd, 256)
    return answer


fd = open_port(DEVICE, BAUD)
print(f"{DEVICE} at {BAUD} baud (Ctrl+C to stop)")

try:
    count = 0
    while True:
        count += 1
        request = f"request {count}\r\n".encode()
        os.write(fd, request)
        answer = read_answer(fd, ANSWER_TIMEOUT)
        if answer:
            print(f"sent {request!r}, received {answer!r}")
        else:
            print(f"sent {request!r}, no answer")
        time.sleep(1.0)
except KeyboardInterrupt:
    pass
finally:
    os.close(fd)
