#!/usr/bin/env python3
"""Client library for the GOcontroll Multibus module protocol.

The module exposes four CDC-ACM ports. Three of them are plain serial bridges to
the connector; the fourth carries a request/response protocol that reaches the
things a serial bridge cannot: the isoSPI cell data, the interface
configuration, and the link counters. This module speaks that protocol.

Nothing outside the standard library is needed. The port is a CDC-ACM tty, so
it is opened with os.open and put into raw mode with termios directly; that
keeps the controller free of a pyserial dependency, the same reason
gocontroll-modulebus.py talks to spidev through raw ioctls.

Typical use:

    from gocontroll_multibus import MultibusLink

    with MultibusLink.open() as link:
        print(link.info())
        snapshot = link.read_cells()
        print(snapshot.cells_v)

Adding a command means adding one entry to COMMANDS and one small method; the
frame handling below does not need to change.
"""
from __future__ import annotations

import glob
import os
import select
import struct
import time
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

try:
    import termios
except ImportError:  # pragma: no cover - only Linux has a tty layer
    # Opening a port needs termios, but the frame helpers below do not, so a
    # non-Linux machine can still exercise the codec against test vectors.
    termios = None

# ── USB identity ─────────────────────────────────────────────────────────────
# candleLight ids, so Linux binds gs_usb to the CAN interface all by itself.
USB_VENDOR_ID = "1d50"
USB_PRODUCT_ID = "606f"

# The protocol lives on CDC port 3, whose control interface is number 7. Finding
# the tty by interface number rather than by name keeps it correct no matter in
# which order the kernel handed out the ttyACM numbers.
PROTOCOL_INTERFACE = 7

#: Symlink the udev rule installs for the protocol port. Preferred over the
#: ttyACM node, whose number depends on enumeration order.
PROTOCOL_SYMLINK = "/dev/mb_protocol"

# CDC control interface number -> connector interface number, for reporting.
UART_INTERFACES = {1: 2, 3: 3, 5: 4}

# ── Frame format ─────────────────────────────────────────────────────────────
# SOF | LEN lo | LEN hi | SEQ | CMD | payload... | CRC lo | CRC hi
# LEN counts SEQ, CMD and the payload. The CRC covers exactly those LEN bytes.
SOF = 0x7E
HEADER_SIZE = 5
CRC_SIZE = 2
MAX_PAYLOAD = 200
MAX_FRAME = HEADER_SIZE + MAX_PAYLOAD + CRC_SIZE
RESPONSE_BIT = 0x80

PROTOCOL_VERSION = 1

# ── Command table ────────────────────────────────────────────────────────────
# Keep this in step with MbProto.h. Extending the protocol is a matter of adding
# a line here and a handler in the firmware.
COMMANDS: Dict[str, int] = {
    "INFO": 0x01,           # -> identity and capabilities
    "IF_GET": 0x02,         # -> interface configuration
    "IF_SET": 0x03,         # <- five mode bytes
    "LINK_STATUS": 0x10,    # -> isoSPI link counters
    "CELLS_READ": 0x20,     # -> cell snapshot
    "CELLS_CONFIG": 0x21,   # <- cell count + poll interval
    "ISOSPI_XFER": 0x30,    # <- raw bytes, -> raw bytes clocked back
    "SIM_SET": 0x40,        # <- cell index + value, simulator build only
}

STATUS_TEXT = {
    0x00: "ok",
    0x01: "unknown command",
    0x02: "bad length",
    0x03: "bad parameter",
    0x04: "not supported in this role",
    0x05: "isoSPI link down",
    0x06: "busy",
}

# ── Interface modes, matching MbIfMode_t ─────────────────────────────────────
MODE_OFF = 0
MODE_CAN = 1
MODE_RS485 = 2
MODE_RS232 = 3
MODE_ISOSPI = 4

MODE_NAMES = {
    MODE_OFF: "off",
    MODE_CAN: "can",
    MODE_RS485: "rs485",
    MODE_RS232: "rs232",
    MODE_ISOSPI: "isospi",
}
MODE_VALUES = {name: value for value, name in MODE_NAMES.items()}

ROLE_NAMES = {0: "master", 1: "slave"}

# ── Cell snapshot flags, matching CellMonitor.h ──────────────────────────────
FLAG_LINK_OK = 0x01
FLAG_STALE = 0x02
FLAG_SIMULATED = 0x04
FLAG_UNDERVOLT = 0x08
FLAG_OVERVOLT = 0x10

MAX_CELLS = 24
MAX_TEMPS = 4


class MultibusError(Exception):
    """Any failure that is not a protocol level status code."""


class MultibusStatusError(MultibusError):
    """The module answered, but with a status other than ok."""

    def __init__(self, command: int, status: int):
        self.command = command
        self.status = status
        text = STATUS_TEXT.get(status, "status 0x%02X" % status)
        super().__init__("command 0x%02X refused: %s" % (command, text))


def crc16(data: bytes) -> int:
    """CRC16/XMODEM: polynomial 0x1021, initial value 0x0000, no reflection."""
    crc = 0
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def build_frame(seq: int, cmd: int, payload: bytes = b"") -> bytes:
    """Render one request frame."""
    if len(payload) > MAX_PAYLOAD:
        raise MultibusError("payload of %d bytes exceeds the %d byte maximum"
                            % (len(payload), MAX_PAYLOAD))
    body = bytes([seq & 0xFF, cmd & 0xFF]) + payload
    return (bytes([SOF]) + struct.pack("<H", len(body)) + body
            + struct.pack("<H", crc16(body)))


def parse_frame(buffer: bytes) -> Tuple[Optional[Tuple[int, int, bytes]], bytes]:
    """Pull the first complete frame out of `buffer`.

    Returns ((seq, cmd, payload), remainder) or (None, remainder). Bytes in
    front of a start byte, and start bytes that do not turn out to begin a valid
    frame, are dropped: that is what lets a reader recover after a partial read
    or a line of noise.
    """
    while True:
        start = buffer.find(bytes([SOF]))
        if start < 0:
            return None, b""
        if start:
            buffer = buffer[start:]

        if len(buffer) < HEADER_SIZE:
            return None, buffer

        body_len = struct.unpack_from("<H", buffer, 1)[0]
        if body_len < 2 or body_len > MAX_PAYLOAD + 2:
            buffer = buffer[1:]          # not a header after all
            continue

        total = body_len + HEADER_SIZE
        if len(buffer) < total:
            return None, buffer          # wait for the rest

        body = buffer[3:3 + body_len]
        carried = struct.unpack_from("<H", buffer, 3 + body_len)[0]
        if crc16(body) != carried:
            buffer = buffer[1:]
            continue

        return (body[0], body[1], body[2:]), buffer[total:]


# ── Decoded answers ──────────────────────────────────────────────────────────

@dataclass
class Info:
    """Answer to INFO."""
    protocol_version: int
    role: int
    hardware: Tuple[int, int, int, int]
    software: Tuple[int, int, int]
    interface_count: int
    protocol_port: int
    max_cells: int
    max_temperatures: int
    max_transfer: int

    @property
    def role_name(self) -> str:
        return ROLE_NAMES.get(self.role, "role %d" % self.role)

    @property
    def hardware_name(self) -> str:
        return "-".join(str(part) for part in self.hardware)

    @property
    def software_name(self) -> str:
        return ".".join(str(part) for part in self.software)

    @classmethod
    def from_bytes(cls, data: bytes) -> "Info":
        if len(data) < 14:
            raise MultibusError("INFO answer is %d bytes, expected 14" % len(data))
        return cls(
            protocol_version=data[0],
            role=data[1],
            hardware=(data[2], data[3], data[4], data[5]),
            software=(data[6], data[7], data[8]),
            interface_count=data[9],
            protocol_port=data[10],
            max_cells=data[11],
            max_temperatures=data[12],
            max_transfer=data[13],
        )


@dataclass
class LinkStatus:
    """Answer to LINK_STATUS: how the isoSPI link is behaving."""
    role: int
    link_ok: bool
    poll_ms: int
    tx_frames: int
    rx_frames: int
    crc_errors: int
    spi_errors: int
    resyncs: int
    last_rx_age_ms: int

    LAYOUT = "<BBHIIIIII"

    @classmethod
    def from_bytes(cls, data: bytes) -> "LinkStatus":
        size = struct.calcsize(cls.LAYOUT)
        if len(data) < size:
            raise MultibusError("LINK_STATUS answer is %d bytes, expected %d"
                                % (len(data), size))
        fields = struct.unpack_from(cls.LAYOUT, data)
        return cls(role=fields[0], link_ok=bool(fields[1]), poll_ms=fields[2],
                   tx_frames=fields[3], rx_frames=fields[4], crc_errors=fields[5],
                   spi_errors=fields[6], resyncs=fields[7], last_rx_age_ms=fields[8])


@dataclass
class CellSnapshot:
    """Answer to CELLS_READ.

    Voltages travel in 100 uV steps, the native unit of the LTC681x family, so
    the numbers are exactly what a real battery monitor would hand over. The
    convenience properties convert to volts for anything that has to be read by
    a human.
    """
    version: int
    flags: int
    sequence: int
    age_ms: int
    pack_mv: int
    cells_100uv: List[int] = field(default_factory=list)
    temperatures_dc: List[int] = field(default_factory=list)

    LAYOUT = "<BBBBIII%dH%dh" % (MAX_CELLS, MAX_TEMPS)

    @property
    def link_ok(self) -> bool:
        return bool(self.flags & FLAG_LINK_OK)

    @property
    def stale(self) -> bool:
        return bool(self.flags & FLAG_STALE)

    @property
    def simulated(self) -> bool:
        return bool(self.flags & FLAG_SIMULATED)

    @property
    def undervoltage(self) -> bool:
        return bool(self.flags & FLAG_UNDERVOLT)

    @property
    def overvoltage(self) -> bool:
        return bool(self.flags & FLAG_OVERVOLT)

    @property
    def cells_v(self) -> List[float]:
        return [value / 10000.0 for value in self.cells_100uv]

    @property
    def temperatures_c(self) -> List[float]:
        return [value / 10.0 for value in self.temperatures_dc]

    @property
    def pack_v(self) -> float:
        return self.pack_mv / 1000.0

    @property
    def spread_v(self) -> float:
        """Difference between the highest and the lowest cell."""
        if not self.cells_100uv:
            return 0.0
        return (max(self.cells_100uv) - min(self.cells_100uv)) / 10000.0

    @classmethod
    def from_bytes(cls, data: bytes) -> "CellSnapshot":
        size = struct.calcsize(cls.LAYOUT)
        if len(data) < size:
            raise MultibusError("CELLS_READ answer is %d bytes, expected %d"
                                % (len(data), size))
        fields = struct.unpack_from(cls.LAYOUT, data)
        n_cells, n_temps = fields[1], fields[2]
        head = 7
        return cls(
            version=fields[0],
            flags=fields[3],
            sequence=fields[4],
            age_ms=fields[5],
            pack_mv=fields[6],
            cells_100uv=list(fields[head:head + min(n_cells, MAX_CELLS)]),
            temperatures_dc=list(fields[head + MAX_CELLS:
                                        head + MAX_CELLS + min(n_temps, MAX_TEMPS)]),
        )


@dataclass
class InterfaceConfig:
    """Answer to IF_GET / IF_SET: the mode of every connector interface."""
    modes: List[int]

    #: Which modes each connector interface accepts, mirroring MbInterfaces.c.
    ALLOWED = {
        1: (MODE_CAN,),
        2: (MODE_RS485, MODE_RS232, MODE_OFF),
        3: (MODE_RS485, MODE_CAN, MODE_OFF),
        4: (MODE_RS485, MODE_CAN, MODE_OFF),
        5: (MODE_ISOSPI, MODE_OFF),
    }

    def name_of(self, interface: int) -> str:
        """Mode of a connector interface, numbered 1..5 as on the connector."""
        return MODE_NAMES.get(self.modes[interface - 1], "?")

    def as_dict(self) -> Dict[str, str]:
        return {"interface%d" % (index + 1): MODE_NAMES.get(mode, "?")
                for index, mode in enumerate(self.modes)}

    @classmethod
    def from_bytes(cls, data: bytes) -> "InterfaceConfig":
        if len(data) < 5:
            raise MultibusError("interface answer is %d bytes, expected 5" % len(data))
        return cls(modes=list(data[:5]))


# ── Port discovery ───────────────────────────────────────────────────────────

def _read_sysfs(path: str) -> str:
    try:
        with open(path) as handle:
            return handle.read().strip()
    except OSError:
        return ""


def enumerate_ports() -> List[Dict[str, object]]:
    """Every CDC-ACM tty that belongs to a Multibus module.

    Each entry carries the device node, the USB interface number and, for the
    three serial bridges, the connector interface they belong to.
    """
    found = []
    for sys_tty in sorted(glob.glob("/sys/class/tty/ttyACM*")):
        device = os.path.join(sys_tty, "device")
        if not os.path.isdir(device):
            continue

        interface_text = _read_sysfs(os.path.join(device, "bInterfaceNumber"))
        if not interface_text:
            continue

        usb_device = os.path.dirname(os.path.realpath(device))
        if (_read_sysfs(os.path.join(usb_device, "idVendor")) != USB_VENDOR_ID or
                _read_sysfs(os.path.join(usb_device, "idProduct")) != USB_PRODUCT_ID):
            continue

        interface = int(interface_text, 16)
        found.append({
            "device": "/dev/" + os.path.basename(sys_tty),
            "usb_interface": interface,
            "connector_interface": UART_INTERFACES.get(interface),
            "is_protocol_port": interface == PROTOCOL_INTERFACE,
            "usb_path": os.path.basename(usb_device),
        })
    return found


#: Interface name per gs_usb channel, installed by 81-gocontroll-multibus.rules.
#: Numbered CAN 1..3 as the knowledge base does, which is NOT the connector
#: interface numbering: CAN 2 is connector interface 3 and CAN 3 is interface 4.
CAN_INTERFACE_NAMES = ["mb_can1", "mb_can2", "mb_can3"]

#: Connector interface each gs_usb channel sits on, for reporting.
CAN_CHANNEL_INTERFACE = {0: 1, 1: 3, 2: 4}


def enumerate_can_interfaces(settle: float = 0.0) -> List[str]:
    """The Linux CAN interfaces of a Multibus module, in gs_usb channel order.

    Prefers the deterministic names the udev rule installs. Without that rule
    the kernel names them in discovery order, which depends on the controller:
    a Moduline IV has four onboard mcp251x controllers so the module lands on
    can4..can6, while an M1 has two and the same module lands on can2..can4.
    Never assume can0.

    The fallback recovers the order from sysfs: gs_usb registers one netdev per
    channel in channel order, so sorting by ifindex - the kernel's creation
    counter - puts them back in that order.

    `settle` is how long to wait for the udev names to turn up. Straight after
    the module enumerates, the netdevs exist for a moment under their kernel
    names before udev renames them; acting in that window picks up names that
    are about to become wrong, and an `ip link set can5 up` a second later then
    fails because can5 no longer exists.
    """
    deadline = time.monotonic() + settle
    while True:
        named = [name for name in CAN_INTERFACE_NAMES
                 if os.path.isdir("/sys/class/net/" + name)]
        if len(named) == len(CAN_INTERFACE_NAMES):
            return named
        if time.monotonic() >= deadline:
            break
        time.sleep(0.1)

    found = []
    for sys_net in glob.glob("/sys/class/net/*"):
        driver = os.path.realpath(os.path.join(sys_net, "device", "driver"))
        if os.path.basename(driver) != "gs_usb":
            continue

        index_text = _read_sysfs(os.path.join(sys_net, "ifindex"))
        if not index_text:
            continue
        found.append((int(index_text), os.path.basename(sys_net)))

    return [name for _index, name in sorted(found)]


def find_protocol_port() -> str:
    """Device node of the protocol port, raising when there is not exactly one."""
    # The udev symlink is unambiguous and survives renumbering, so prefer it.
    if os.path.exists(PROTOCOL_SYMLINK):
        return PROTOCOL_SYMLINK

    candidates = [entry for entry in enumerate_ports() if entry["is_protocol_port"]]
    if not candidates:
        raise MultibusError(
            "no Multibus protocol port found. Check that the module enumerated "
            "(lsusb should list %s:%s) and that cdc_acm is loaded."
            % (USB_VENDOR_ID, USB_PRODUCT_ID))
    if len(candidates) > 1:
        nodes = ", ".join(str(entry["device"]) for entry in candidates)
        raise MultibusError("more than one Multibus module present: %s. "
                            "Name the one you want explicitly." % nodes)
    return str(candidates[0]["device"])


# ── The link ─────────────────────────────────────────────────────────────────

class MultibusLink:
    """One open connection to the protocol port of a Multibus module."""

    def __init__(self, device: str, timeout: float = 1.0, retries: int = 2):
        self.device = device
        self.timeout = timeout
        self.retries = retries
        self._seq = 0
        self._buffer = b""

        self._fd = os.open(device, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        try:
            self._configure_raw()
        except Exception:
            os.close(self._fd)
            raise

    @classmethod
    def open(cls, device: Optional[str] = None, **kwargs) -> "MultibusLink":
        """Open the given device, or the only Multibus protocol port present."""
        return cls(device or find_protocol_port(), **kwargs)

    # -- context manager -----------------------------------------------------
    def __enter__(self) -> "MultibusLink":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()

    def close(self) -> None:
        """Release the port. Safe to call more than once, and from a handler.

        The descriptor is forgotten *before* it is closed, so a KeyboardInterrupt
        landing between the two cannot leave a stale number behind for a second
        close() to trip over with EBADF - which is exactly what a Ctrl+C during
        cleanup used to produce.

        Queued output is discarded first. Closing a tty makes the kernel wait for
        its write buffer to drain, and on a CDC port whose far end is not reading
        that wait never ends - the process parks in tty_wait_until_sent forever,
        with the descriptor already gone from its table. That is easy to hit here:
        a request that timed out on write did so precisely because the buffer was
        full, and closing straight afterwards would hang on those same bytes.

        Catching Exception rather than OSError is deliberate: termios.error does
        not derive from OSError, so `except OSError` lets it through. On a port
        whose USB device has just gone away tcflush raises exactly that - EIO
        wrapped in a termios.error - and it escaped all the way out of the
        daemon, which died instead of recovering from a reset module.
        """
        fd, self._fd = self._fd, -1
        if fd < 0:
            return

        if termios is not None:
            try:
                termios.tcflush(fd, termios.TCIOFLUSH)
            except Exception:
                pass
        try:
            os.close(fd)
        except Exception:
            pass

    # -- tty plumbing --------------------------------------------------------
    def _configure_raw(self) -> None:
        """Put the tty in raw 8N1 mode.

        The baud rate is meaningless here: behind this port sits the protocol
        handler, not a UART. What matters is that the kernel passes every byte
        through untouched, so no echo, no CR/LF translation and no flow control.
        """
        iflag, oflag, cflag, lflag, ispeed, ospeed, cc = termios.tcgetattr(self._fd)

        iflag &= ~(termios.IGNBRK | termios.BRKINT | termios.PARMRK |
                   termios.ISTRIP | termios.INLCR | termios.IGNCR |
                   termios.ICRNL | termios.IXON | termios.IXOFF | termios.IXANY)
        oflag &= ~termios.OPOST
        lflag &= ~(termios.ECHO | termios.ECHOE | termios.ECHONL |
                   termios.ICANON | termios.ISIG | termios.IEXTEN)
        cflag &= ~(termios.CSIZE | termios.PARENB | termios.CSTOPB)
        cflag |= termios.CS8 | termios.CLOCAL | termios.CREAD

        cc = list(cc)
        cc[termios.VMIN] = 0
        cc[termios.VTIME] = 0

        termios.tcsetattr(self._fd, termios.TCSANOW,
                          [iflag, oflag, cflag, lflag, ispeed, ospeed, cc])
        termios.tcflush(self._fd, termios.TCIOFLUSH)

    def _write(self, data: bytes) -> None:
        while data:
            _, writable, _ = select.select([], [self._fd], [], self.timeout)
            if not writable:
                raise MultibusError("timed out writing to %s" % self.device)
            data = data[os.write(self._fd, data):]

    def _read_frame(self, deadline: float) -> Optional[Tuple[int, int, bytes]]:
        """Return the next valid frame, or None when the deadline passes."""
        while True:
            frame, self._buffer = parse_frame(self._buffer)
            if frame is not None:
                return frame

            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None

            readable, _, _ = select.select([self._fd], [], [], remaining)
            if not readable:
                return None

            chunk = os.read(self._fd, 512)
            if not chunk:
                # A CDC-ACM node returns nothing on a disconnected device.
                raise MultibusError("%s went away" % self.device)
            self._buffer += chunk

    # -- protocol ------------------------------------------------------------
    def request(self, command: int, payload: bytes = b"") -> bytes:
        """Send one request and return the answer payload without its status.

        Retries on timeout: a request that was lost costs one round trip, and
        the sequence number keeps a late answer to an earlier request from being
        mistaken for this one.
        """
        last_error: Optional[Exception] = None

        for _ in range(self.retries + 1):
            self._seq = (self._seq + 1) & 0xFF
            seq = self._seq
            self._buffer = b""

            self._write(build_frame(seq, command, payload))

            deadline = time.monotonic() + self.timeout
            while True:
                frame = self._read_frame(deadline)
                if frame is None:
                    last_error = MultibusError(
                        "no answer to command 0x%02X within %.1fs" % (command, self.timeout))
                    break

                answer_seq, answer_cmd, answer_payload = frame
                if answer_seq != seq or answer_cmd != (command | RESPONSE_BIT):
                    continue          # stale answer to an earlier request

                if not answer_payload:
                    last_error = MultibusError("empty answer to command 0x%02X" % command)
                    break

                status = answer_payload[0]
                if status != 0:
                    raise MultibusStatusError(command, status)
                return answer_payload[1:]

        raise last_error if last_error else MultibusError("request failed")

    # -- commands ------------------------------------------------------------
    def info(self) -> Info:
        return Info.from_bytes(self.request(COMMANDS["INFO"]))

    def link_status(self) -> LinkStatus:
        return LinkStatus.from_bytes(self.request(COMMANDS["LINK_STATUS"]))

    def read_cells(self) -> CellSnapshot:
        return CellSnapshot.from_bytes(self.request(COMMANDS["CELLS_READ"]))

    def get_interfaces(self) -> InterfaceConfig:
        return InterfaceConfig.from_bytes(self.request(COMMANDS["IF_GET"]))

    def set_interfaces(self, modes: List[int]) -> InterfaceConfig:
        """Apply an interface configuration.

        The module answers with the configuration that is active afterwards, so
        a rejected combination is visible as "nothing changed" rather than as a
        silent success.
        """
        if len(modes) != 5:
            raise MultibusError("expected 5 interface modes, got %d" % len(modes))
        return InterfaceConfig.from_bytes(
            self.request(COMMANDS["IF_SET"], bytes(modes)))

    def configure_cells(self, cell_count: int, poll_ms: int) -> None:
        self.request(COMMANDS["CELLS_CONFIG"],
                     struct.pack("<BH", cell_count, poll_ms))

    def isospi_transfer(self, data: bytes) -> bytes:
        """Clock raw bytes through the LTC6820 within one chip select.

        This bypasses the frame protocol entirely and is the hook for driving a
        real LTC681x battery monitor from Python before any of it is written in
        firmware.
        """
        return self.request(COMMANDS["ISOSPI_XFER"], data)

    def simulate_cell(self, index: int, millivolts: float) -> None:
        """Pin one simulated cell to a fixed voltage. Simulator build only."""
        value = int(round(millivolts * 10))     # mV -> 100 uV steps
        if not 0 <= value <= 0xFFFF:
            raise MultibusError("%.1f mV is outside the range a cell can carry"
                                % millivolts)
        self.request(COMMANDS["SIM_SET"], struct.pack("<BH", index, value))
