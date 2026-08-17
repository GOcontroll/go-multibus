#!/usr/bin/env python3
"""Bring-up and supervision logic for a Multibus module.

The daemon and the UI both drive a module through the same state machine, so it
lives here rather than in either of them.

The sequence a module goes through after a controller boot or a module reset:

    RESET        pulse the slot reset line
      |          the STM32 comes up in the GOcontroll bootloader
    START        tell the bootloader its firmware is current, so it jumps to
      |          the application. Without this the module sits in the
      |          bootloader forever and never enumerates on USB.
    CONFIGURE    push the interface configuration over the module bus (SPI)
      |          This happens over SPI and not over USB on purpose: the
      |          controller has to know which interfaces are real before it
      |          initialises them, and USB is not up yet at this point.
    ENUMERATE    wait for the USB device to appear and its four CDC ports with
      |          it. The descriptor is fixed, so this always yields the same
      |          set of ports regardless of the configuration.
    RUNNING      poll the isoSPI cell data over USB and publish it

Every step is idempotent and the whole sequence is re-entered from the top when
the module disappears, which is what makes recovery after a reset or a re-plug
automatic rather than something an operator has to do.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from gocontroll_multibus import (  # noqa: E402
    MODE_CAN,
    MODE_ISOSPI,
    MODE_NAMES,
    MODE_RS485,
    MODE_VALUES,
    CellSnapshot,
    MultibusError,
    MultibusLink,
    PROTOCOL_SYMLINK,
    enumerate_can_interfaces,
    enumerate_ports,
)

# ── Configuration ────────────────────────────────────────────────────────────

CONFIG_PATH = "/etc/gocontroll/multibus.json"
OUTPUT_PATH = "/dev/shm/gocontroll/multibus-cells.json"
STATE_PATH = "/dev/shm/gocontroll/multibus-state.json"

DEFAULT_CONFIG = {
    "slot": 8,
    "interfaces": ["can", "rs485", "can", "rs485", "isospi"],
    "cell_count": 12,
    "module_poll_ms": 100,
    "publish_interval_s": 0.5,
    "bring_up_can": True,
    "can_bitrate": 500000,
    "output": OUTPUT_PATH,
}

# How long to wait for USB to enumerate after the application starts. The STM32
# needs to run its init, attach, and be enumerated by the host; on a Moduline
# that settles in well under two seconds, and five leaves room for a slow boot.
ENUMERATION_TIMEOUT = 5.0

# How long to let udev finish naming the module's devices after they appear.
# Renaming three netdevs and adding four symlinks takes a few tens of
# milliseconds on a Moduline; two seconds is a ceiling, not a wait.
UDEV_SETTLE = 2.0
ENUMERATION_POLL = 0.2

# A module that stops answering is given this long before the whole sequence is
# restarted from RESET. Short enough to recover quickly, long enough that a
# single dropped poll does not trigger a reset.
FAILURE_GRACE = 3.0


class State:
    """The steps of the bring-up sequence, in the order they run."""
    RESET = "reset"
    START = "start"
    CONFIGURE = "configure"
    ENUMERATE = "enumerate"
    RUNNING = "running"
    FAILED = "failed"


@dataclass
class Config:
    slot: int = 8
    interfaces: List[str] = field(
        default_factory=lambda: list(DEFAULT_CONFIG["interfaces"]))
    cell_count: int = 12
    module_poll_ms: int = 100
    publish_interval_s: float = 0.5
    bring_up_can: bool = True
    # Fallback only. Bus parameters belong to go-can; this is used solely when
    # go-can is not installed or has no config for the interface yet.
    can_bitrate: int = 500000
    output: str = OUTPUT_PATH

    @property
    def interface_modes(self) -> List[int]:
        """The configuration as the mode bytes the module expects."""
        try:
            return [MODE_VALUES[name] for name in self.interfaces]
        except KeyError as unknown:
            raise MultibusError(
                "unknown interface mode %s in %s; pick from %s"
                % (unknown, CONFIG_PATH, ", ".join(sorted(MODE_VALUES))))

    @property
    def can_channels(self) -> List[int]:
        """Which gs_usb channels have a live transceiver behind them.

        The module always presents three channels. Channel 0 is connector
        interface 1 and is always CAN; channels 1 and 2 follow interfaces 3 and
        4. Bringing up a channel whose interface is RS485 would give a netdev
        that can never carry a frame.

        These are channel *indices*, not interface names - see
        enumerate_can_interfaces() for why the two are not interchangeable.
        """
        channels = [0]
        for channel, interface in ((1, 2), (2, 3)):
            if self.interface_modes[interface] == MODE_CAN:
                channels.append(channel)
        return channels

    @classmethod
    def load(cls, path: str = CONFIG_PATH) -> "Config":
        values = dict(DEFAULT_CONFIG)
        try:
            with open(path) as handle:
                values.update(json.load(handle))
        except FileNotFoundError:
            pass          # the defaults are a working configuration
        except (OSError, ValueError) as error:
            raise MultibusError("cannot read %s: %s" % (path, error))

        known = {key: values[key] for key in DEFAULT_CONFIG if key in values}
        return cls(**known)

    def save(self, path: str = CONFIG_PATH) -> None:
        directory = os.path.dirname(path)
        if directory:
            os.makedirs(directory, exist_ok=True)

        payload = {
            "slot": self.slot,
            "interfaces": self.interfaces,
            "cell_count": self.cell_count,
            "module_poll_ms": self.module_poll_ms,
            "publish_interval_s": self.publish_interval_s,
            "bring_up_can": self.bring_up_can,
            "can_bitrate": self.can_bitrate,
            "output": self.output,
        }
        temporary = path + ".tmp"
        with open(temporary, "w") as handle:
            json.dump(payload, handle, indent=2)
            handle.write("\n")
        os.replace(temporary, path)


def write_atomic(path: str, document: dict) -> None:
    """Replace `path` in one step so a reader never sees a partial document."""
    directory = os.path.dirname(path)
    if directory:
        os.makedirs(directory, exist_ok=True)

    temporary = path + ".tmp"
    with open(temporary, "w") as handle:
        json.dump(document, handle, separators=(",", ":"))
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


# ── Module bus helpers ───────────────────────────────────────────────────────
# Imported lazily: the module bus tool talks to spidev, which only exists on a
# controller, and the UI must still run on a machine where it does not.

#: Where gocontroll-modulebus lives. It is a script with a hyphen in its name
#: rather than an importable module, so it is loaded by path rather than
#: imported. Checked in order: a checkout next to this file wins over an
#: installed copy, so development on a controller that also has the package
#: installed picks up the checkout.
_LIB_DIR = os.path.dirname(os.path.abspath(__file__))
MODULE_BUS_PATHS = [
    # Checkout: this file is in lib/, the script in bin/.
    os.path.join(os.path.dirname(_LIB_DIR), "bin", "gocontroll-modulebus"),
    # Flat layout, everything in one directory.
    os.path.join(_LIB_DIR, "gocontroll-modulebus.py"),
    # Installed.
    "/usr/bin/gocontroll-modulebus",
]


def _module_bus():
    import importlib.util
    from importlib.machinery import SourceFileLoader

    path = next((p for p in MODULE_BUS_PATHS if os.path.exists(p)), None)
    if path is None:
        raise MultibusError("cannot find gocontroll-modulebus; looked in %s"
                            % ", ".join(MODULE_BUS_PATHS))

    # The loader has to be named explicitly. Installed, the script is
    # /usr/bin/gocontroll-modulebus with no extension, and importlib refuses to
    # guess a loader for a filename it does not recognise - spec_from_file_location
    # then quietly returns None.
    loader = SourceFileLoader("gocontroll_modulebus", path)
    spec = importlib.util.spec_from_file_location("gocontroll_modulebus", path,
                                                  loader=loader)
    if spec is None:
        raise MultibusError("cannot load %s" % path)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


# ── The supervisor ───────────────────────────────────────────────────────────

class ModuleSupervisor:
    """Drives one module through bring-up and keeps it publishing.

    `step()` does a bounded amount of work and returns; the caller decides how
    often to call it. That keeps the daemon and the UI on the same code without
    either of them needing threads.
    """

    def __init__(self, config: Config,
                 log: Optional[Callable[[str], None]] = None,
                 start_state: str = State.RESET,
                 auto_reset: bool = True,
                 link_timeout: float = 1.0,
                 link_retries: int = 2,
                 enumeration_timeout: float = ENUMERATION_TIMEOUT):
        self.config = config
        # How long a single step may block. The daemon can afford to wait; the
        # UI cannot, because every second spent here is a second it does not
        # repaint or read the keyboard.
        self.link_timeout = link_timeout
        self.link_retries = link_retries
        self.enumeration_timeout = enumeration_timeout
        # Where the sequence begins. The daemon starts at RESET because a
        # controller boot should leave the module in a known state. The UI
        # starts at ENUMERATE so that merely opening it to look at a running
        # machine does not reset the module underneath it.
        self.start_state = start_state
        # Whether a failure may restart the sequence from RESET on its own.
        self.auto_reset = auto_reset
        self.state = start_state
        self.log = log or (lambda message: None)

        self.link: Optional[MultibusLink] = None
        self.snapshot: Optional[CellSnapshot] = None
        self.last_error = ""
        self.failures = 0
        self.applied_interfaces: List[int] = []
        # Set when the module bus would not take the configuration, so the
        # enumerate step applies it over USB instead.
        self.configure_over_usb = False
        self.module_running = False
        self._failing_since: Optional[float] = None
        self._last_publish = 0.0

    # -- lifecycle -----------------------------------------------------------
    def close(self) -> None:
        if self.link is not None:
            self.link.close()
            self.link = None

    def restart(self, full: bool = False) -> None:
        """Go back to the start of the sequence.

        `full` forces a hardware reset even for a supervisor that normally
        attaches without one; that is what the UI's restart key asks for.
        """
        self.close()
        self.state = State.RESET if full else self.start_state
        self.module_running = False
        self.snapshot = None
        self._failing_since = None

    # -- individual steps ----------------------------------------------------
    def _do_reset(self) -> None:
        bus = _module_bus()
        self.log("resetting slot %d" % self.config.slot)
        bus.pulse_reset(self.config.slot)
        self.state = State.START

    def _do_start(self) -> None:
        bus = _module_bus()
        handle = bus.open_spidev(bus.SLOT_SPIDEV[self.config.slot])

        if bus.identify(handle) is None:
            raise MultibusError(
                "slot %d does not answer on the module bus. Either nothing is "
                "in the slot, or the reset line does not reach the STM32."
                % self.config.slot)

        # The bootloader jumps the moment it accepts this, so there is no reply.
        bus.transfer(handle, bus.build_message(bus.MSG_UP_TO_DATE))
        self.log("told the bootloader to start the application")
        handle.close()

        self.state = State.CONFIGURE

    def _do_configure(self) -> None:
        """Push the interface configuration over SPI, before USB comes up."""
        bus = _module_bus()
        modes = self.config.interface_modes
        handle = bus.open_spidev(bus.SLOT_SPIDEV[self.config.slot])

        try:
            reply = bus.request(handle, bus.MSG_IF_SET, bytes(modes))
            if reply is None:
                # Not fatal. Firmware built before ModuleBus.c serves the module
                # bus from the bootloader only, so an application that ignores
                # us here may still speak the protocol over USB. Carry on and
                # configure there instead; the controller learns the interface
                # layout a little later than it would have, which costs nothing
                # because the USB descriptor is fixed either way.
                self.log("no module bus answer; will configure over USB instead")
                self.configure_over_usb = True
                self.state = State.ENUMERATE
                return
            if reply[3] != 0:
                raise MultibusError(
                    "the module refused the interface combination %s"
                    % ",".join(self.config.interfaces))

            self.applied_interfaces = list(reply[4:9])
            self.configure_over_usb = False
            self.log("interfaces configured over SPI: %s"
                     % ",".join(MODE_NAMES.get(mode, "?")
                                for mode in self.applied_interfaces))
        finally:
            handle.close()

        self.module_running = True
        self.state = State.ENUMERATE

    def _do_enumerate(self) -> None:
        """Wait for USB, then open the protocol port and apply the cell config."""
        deadline = time.monotonic() + self.enumeration_timeout

        while True:
            ports = enumerate_ports()
            if any(entry["is_protocol_port"] for entry in ports):
                break
            if time.monotonic() >= deadline:
                raise MultibusError(
                    "the module did not enumerate on USB within %.1fs. Check "
                    "that the controller USB port is in host mode."
                    % self.enumeration_timeout)
            time.sleep(ENUMERATION_POLL)

        # Same race as the CAN names: the ttyACM nodes exist before udev has
        # added /dev/mb_protocol. Waiting means the log names the stable device
        # rather than whichever ttyACM number this enumeration happened to get.
        settle = time.monotonic() + UDEV_SETTLE
        while not os.path.exists(PROTOCOL_SYMLINK) and time.monotonic() < settle:
            time.sleep(0.1)

        # Opening is part of the wait, not a step after it. Right after the
        # module is reset the device nodes from before the reset can still be
        # around for a moment: the name exists, but opening it gives ENODEV
        # because the USB device behind it is gone. Treating that as a hard
        # failure threw the whole sequence away and started over - visible at
        # boot as an "enumerate failed: [Errno 19] No such device" followed by a
        # second, successful pass. Retrying until the deadline rides it out.
        self.link = None
        while True:
            try:
                self.link = MultibusLink.open(timeout=self.link_timeout,
                                              retries=self.link_retries)
                break
            except (MultibusError, OSError) as failure:
                if time.monotonic() >= deadline:
                    raise MultibusError(
                        "the protocol port did not become usable within %.1fs "
                        "(%s)" % (self.enumeration_timeout, failure))
                time.sleep(ENUMERATION_POLL)

        self.log("connected to %s" % self.link.device)

        # The port existing proves the module enumerated, so a failure from here
        # on is about what the firmware serves rather than about USB. Say that,
        # because "not reachable" sends people looking at cables when the real
        # answer is that the module is running a build from before the protocol
        # existed - which is exactly what a stock 0.0.1 image does.
        try:
            info = self.link.info()
        except MultibusError as failure:
            raise MultibusError(
                "%s enumerated but does not answer the module protocol (%s). "
                "The firmware predates MbProto - on such a build this port is a "
                "plain RS485 bridge. Flash a current image."
                % (self.link.device, failure))

        self.log("module is a %s, hardware %s, software %s"
                 % (info.role_name, info.hardware_name, info.software_name))

        # Fall back to configuring over USB when the module bus would not take
        # it. Failing here is fatal: at this point the module clearly does speak
        # the protocol, so a refusal is a real answer about the configuration.
        if self.configure_over_usb:
            applied = self.link.set_interfaces(self.config.interface_modes)
            if applied.modes != self.config.interface_modes:
                raise MultibusError(
                    "the module refused the interface combination %s; it kept %s"
                    % (",".join(self.config.interfaces),
                       ",".join(applied.as_dict().values())))
            self.log("interfaces configured over USB: %s"
                     % ",".join(self.config.interfaces))
            self.configure_over_usb = False

        self.link.configure_cells(self.config.cell_count,
                                  self.config.module_poll_ms)

        # Read back what the module actually has, which also covers the case
        # where the SPI configuration did not land.
        self.applied_interfaces = self.link.get_interfaces().modes

        if self.config.bring_up_can:
            self._bring_up_can()

        self.state = State.RUNNING
        self.failures = 0
        self._failing_since = None

    def _bring_up_can(self) -> None:
        """Bring up only the CAN interfaces that have a transceiver behind them.

        The names are looked up rather than assumed: a Moduline has its own
        mcp251x controllers on can0..can3, so the module's channels land on
        can4..can6. Guessing "can0" would reconfigure the controller's own bus.

        The bus parameters are go-can's to decide, not ours - it is the canonical
        CAN configuration tool on these controllers and keeps them in
        /etc/gocontroll/can.d/. So the interface is handed to `go-can apply`,
        which reads that config. Setting a bitrate here as well would make the
        two tools disagree: go-can would keep reporting its stored value while
        the interface actually ran at ours.
        """
        # Give udev a moment to finish renaming; see enumerate_can_interfaces.
        interfaces = enumerate_can_interfaces(settle=UDEV_SETTLE)
        if len(interfaces) < 3:
            self.log("expected 3 gs_usb interfaces, found %d (%s); not touching CAN"
                     % (len(interfaces), ", ".join(interfaces) or "none"))
            return

        for channel in self.config.can_channels:
            name = interfaces[channel]
            if self._apply_can_with_go_can(name):
                continue
            self._apply_can_with_ip(name)

    def _apply_can_with_go_can(self, name: str) -> bool:
        """Let go-can configure and raise the interface. False if it could not.

        Fails when go-can is absent, or when it has no config for this interface
        yet - a fresh controller that has never had a module in it. The caller
        falls back in both cases.
        """
        try:
            result = subprocess.run(["go-can", "apply", name],
                                    capture_output=True, timeout=15)
        except (subprocess.SubprocessError, FileNotFoundError, OSError):
            return False

        if result.returncode != 0:
            return False

        self.log("%s up, configured by go-can" % name)
        return True

    def _apply_can_with_ip(self, name: str) -> None:
        """Raise the interface directly, for a controller without go-can."""
        try:
            subprocess.run(["ip", "link", "set", name, "down"],
                           check=False, capture_output=True)
            subprocess.run(["ip", "link", "set", name, "up", "type", "can",
                            "bitrate", str(self.config.can_bitrate)],
                           check=True, capture_output=True)
            self.log("%s up at %d bit/s (go-can not available)"
                     % (name, self.config.can_bitrate))
        except (subprocess.CalledProcessError, FileNotFoundError) as error:
            # A CAN interface that will not come up is worth reporting but
            # must not stop the cell data from being published.
            self.log("could not bring up %s: %s" % (name, error))

    def _do_running(self) -> None:
        assert self.link is not None

        now = time.monotonic()
        if (now - self._last_publish) < self.config.publish_interval_s:
            return
        self._last_publish = now

        self.snapshot = self.link.read_cells()
        status = self.link.link_status()
        document = snapshot_document(self.snapshot, status, self.link.device,
                                     self.applied_interfaces)
        document["errors"] = {"read_failures": self.failures,
                              "last_error": self.last_error}
        write_atomic(self.config.output, document)

        self.failures = 0
        self.last_error = ""
        self._failing_since = None

    # -- the step the caller drives ------------------------------------------
    HANDLERS: Dict[str, str] = {
        State.RESET: "_do_reset",
        State.START: "_do_start",
        State.CONFIGURE: "_do_configure",
        State.ENUMERATE: "_do_enumerate",
        State.RUNNING: "_do_running",
    }

    def step(self) -> None:
        """Advance the state machine by one bounded piece of work."""
        try:
            handler = self.HANDLERS.get(self.state)
            if handler is None:
                # FAILED with auto_reset off: stay put until asked to retry.
                if not self.auto_reset:
                    return
                self.restart()
                return
            getattr(self, handler)()

        except (MultibusError, OSError, subprocess.SubprocessError) as error:
            self.failures += 1
            self.last_error = str(error)
            self.log("%s failed: %s" % (self.state, error))

            now = time.monotonic()
            if self._failing_since is None:
                self._failing_since = now

            self.close()

            # Publish the failure: a reader that only ever sees a stale file
            # cannot tell a dead service from a dead battery link.
            write_atomic(self.config.output, {
                "schema": 1,
                "timestamp": time.time(),
                "valid": False,
                "link_ok": False,
                "state": self.state,
                "errors": {"read_failures": self.failures,
                           "last_error": self.last_error},
            })

            # Failing inside RUNNING gets a grace period, because a single
            # dropped poll is not a reason to reset a working module. Failing
            # during bring-up restarts the sequence straight away.
            if self.state == State.RUNNING:
                if (now - self._failing_since) < FAILURE_GRACE:
                    self.state = State.ENUMERATE
                    return

            if not self.auto_reset:
                # The UI must not reset hardware on its own; park in a state the
                # operator can see and let them press the restart key.
                self.state = State.FAILED
                return

            self.restart()

        finally:
            self._publish_state()

    def _publish_state(self) -> None:
        """A small document describing where the bring-up sequence stands."""
        try:
            write_atomic(STATE_PATH, {
                "schema": 1,
                "timestamp": time.time(),
                "state": self.state,
                "slot": self.config.slot,
                "module_running": self.module_running,
                "interfaces": [MODE_NAMES.get(mode, "?")
                               for mode in self.applied_interfaces],
                "failures": self.failures,
                "last_error": self.last_error,
            })
        except OSError:
            pass          # a state file we cannot write must not stop the loop


def snapshot_document(snapshot: CellSnapshot, status, device: str,
                      interfaces: List[int]) -> dict:
    """The document published to /dev/shm.

    Voltages appear twice on purpose: `cells_v` for anything that displays them,
    `cells_100uv` for anything that wants the raw device values back without a
    rounding step in between.
    """
    voltages = snapshot.cells_v
    return {
        "schema": 1,
        "timestamp": time.time(),
        "source": {"device": device,
                   "interfaces": [MODE_NAMES.get(mode, "?") for mode in interfaces]},
        "valid": snapshot.link_ok and not snapshot.stale,
        "link_ok": snapshot.link_ok,
        "stale": snapshot.stale,
        "simulated": snapshot.simulated,
        "sequence": snapshot.sequence,
        "age_ms": snapshot.age_ms,
        "cell_count": len(voltages),
        "cells_v": [round(value, 4) for value in voltages],
        "cells_100uv": snapshot.cells_100uv,
        "pack_v": round(snapshot.pack_v, 3),
        "min_v": round(min(voltages), 4) if voltages else None,
        "max_v": round(max(voltages), 4) if voltages else None,
        "spread_v": round(snapshot.spread_v, 4),
        "temperatures_c": [round(value, 1) for value in snapshot.temperatures_c],
        "alarms": {"undervoltage": snapshot.undervoltage,
                   "overvoltage": snapshot.overvoltage},
        "link": {"poll_ms": status.poll_ms,
                 "tx_frames": status.tx_frames,
                 "rx_frames": status.rx_frames,
                 "crc_errors": status.crc_errors,
                 "spi_errors": status.spi_errors,
                 "resyncs": status.resyncs,
                 "last_rx_age_ms": status.last_rx_age_ms},
    }
