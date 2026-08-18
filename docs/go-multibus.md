# go-multibus: the Multibus service on the controller

`go-multibus` brings a Multibus module up after a controller boot, configures
its interfaces, and keeps the battery cell data flowing to the rest of the
system. It is the piece that makes the module work without anyone typing
anything.

It has two faces over one state machine: a background service that runs at boot,
and a terminal UI for commissioning a machine. What the UI shows is exactly what
the service does, because both drive the same code.

## The problem it solves

A freshly powered module does not do anything useful on its own:

- It comes up in the GOcontroll bootloader and waits there. `go-modules scan`
  does not start the application, and `go-modules update` only starts it as a
  side effect of a firmware upload. Without a deliberate hand-over the module
  never enumerates on USB at all.
- Once running, its interfaces default to a configuration that is almost
  certainly not what the machine needs. Interface 3 might have to be CAN where
  the default is RS485.
- The controller has to know which of the three CAN channels and which of the
  three serial ports actually have a transceiver behind them, so it can bring up
  the right network interfaces and leave the others alone.

The order matters, and it is the reason the configuration goes over SPI:

```
   reset ──► start ──► configure ──► enumerate ──► running
             (SPI)      (SPI)          (USB)        (USB)
                          ▲                ▲
                          │                └─ by now the controller knows which
                          │                   canX and ttyACMx are meaningful
                          └─ has to happen before USB comes up
```

## The five states

| State       | What happens                                                        |
|-------------|---------------------------------------------------------------------|
| `reset`     | Pulse the slot reset line. The STM32 restarts into the bootloader.   |
| `start`     | Identify the module, then send the bootloader `UP_TO_DATE` so it jumps to the application. |
| `configure` | Send `IF_SET` over the module bus with the five interface modes.     |
| `enumerate` | Wait for the USB device and its four CDC ports, then open the protocol port and apply the cell configuration. |
| `running`   | Poll the cell snapshot and publish it.                               |

Every step is idempotent, and the sequence is re-entered from the top whenever
the module disappears. That is what makes recovery automatic: pull the module
and put it back, and the service resets, restarts, reconfigures and resumes
without anyone intervening.

A failure inside `running` gets a three second grace period before the module is
reset, because a single dropped USB poll is not a reason to restart a working
module. A failure during bring-up restarts the sequence immediately.

## Interfaces over SPI, cell data over USB

Two links to the same module, each doing what it is good at:

**The module bus (SPI)** exists before USB and stays available regardless of
whether USB enumerated. It carries the interface configuration and a compact
status message. `ModuleBus.c` serves it in the application, using the same 46
byte message format as the bootloader so `gocontroll-modulebus` needs no
second code path.

Because SPI is full duplex, the module cannot answer in the transfer that
carries the request - it only learns what was asked once the last byte is
clocked in. The reply goes out with the *next* transfer, so the Python side
sends the same message repeatedly until the matching answer comes back. That is
safe because every application message is idempotent.

**USB** carries the cell data, which is far too large and too frequent for a 46
byte message. See [multibus-usb-protocol.md](multibus-usb-protocol.md).

## Running it

### As a service

```bash
install -m 755 bin/go-multibus        /usr/bin/go-multibus
install -m 755 bin/gocontroll-cellmon  /usr/bin/gocontroll-cellmon
install -m 755 bin/gocontroll-modulebus /usr/bin/gocontroll-modulebus
install -d /usr/lib/gocontroll-multibus
install -m 644 lib/gocontroll_multibus.py /usr/lib/gocontroll-multibus/
install -m 644 lib/gocontroll_multibus_service.py /usr/lib/gocontroll-multibus/
install -m 644 debian/systemd/go-multibus.service /etc/systemd/system/
systemctl enable --now go-multibus
```

The entry points drop their `.py` so they read like `go-can` and `go-modules`.
The shared modules go to `/usr/lib/gocontroll-multibus`, which every script adds
to its import path - along with its own directory, so a checkout still runs
without installing anything.

Only run one of `go-multibus daemon` and `gocontroll-cellmon serve` at a time.
Both open the protocol port, and two processes writing to one CDC port interleave
their requests so each sees the other's answers.

### The UI

```console
$ go-multibus
```

The UI follows go-can: same orange title, same rule lines, same cyan caret on
the selected row, green for the value in force, and a dark-grey key hint at the
bottom. Navigation is the same too - move with the arrows, Enter to descend into
a picker, Left or Esc to come back, q or Esc to leave.

```
  GOcontroll Moduline Multibus tool  v0.1.0
  ------------------------------------
  Module: slot 8   running
  ------------------------------------
    Interface 1  6/5      CAN       fixed
    Interface 2  12/13    RS485
  > Interface 3  18/19    CAN
    Interface 4  24/25    RS485
    Interface 5  isoSPI   isoSPI
  ------------------------------------
  up/down navigate   Enter select mode   c cells   r reset   q quit
```

Enter on an interface opens its mode picker, listing only what that interface
can actually be:

```
  Interface 3  (pins 18/19)  -  Select Mode

    RS485
  > CAN
    Off
  ------------------------------------
  up/down navigate   Enter apply (resets module)   left/Esc back
```

Enter applies the mode: it is written to the configuration file *and* put into
effect, the way go-can's bitrate picker saves and configures in one keypress.
Applying implies a module reset, because the configuration travels over the
module bus, which is only read before USB comes up. The hint line says so.

Interface 1 and 5 are fixed, so their picker has a single entry and a note
saying it is not configurable.

`c` opens the cell voltages, `Esc` or Left comes back:

```
  Cells  -  pack 43.464 V   spread 0.0440 V   SIMULATED

   1 3.6000v    2 3.6040     3 3.6080     4 3.6120
   5 3.6160     6 3.6200     7 3.6240     8 3.6280
   9 3.6320    10 3.6360    11 3.6400    12 3.6440^
```

**Opening the UI does not reset the module.** It attaches to whatever is already
running and starts polling, so it is safe to open on a live machine just to look.
Only `r` and Enter-in-the-picker touch the hardware, and both say so. If the
module cannot be reached the UI parks at `not reachable - press r to reset the
module` rather than resetting on its own. The daemon behaves the opposite way,
because at boot a reset is exactly what is wanted.

When the module reports a different mode than the one on screen, the row says
so: `Interface 3  18/19  CAN   module has RS485`. A configuration the module
refused is visible immediately instead of looking like it was applied.

### One-shot commands

```console
$ go-multibus status
state      : running
slot       : 8
interfaces : can, rs485, can, rs485, isospi
cells      : 12, pack 43.464 V, spread 0.0440 V (simulated)

$ go-multibus config
$ go-multibus set 3 can
```

`set` writes the configuration file and tells you to restart the service; it
does not touch a running module, so a scripted change and a running machine
never disagree about what is happening.

## Configuration file

`/etc/gocontroll/multibus.json`. Missing keys fall back to the defaults, and a
missing file is itself a working configuration.

```json
{
  "slot": 8,
  "interfaces": ["can", "rs485", "can", "rs485", "isospi"],
  "cell_count": 12,
  "module_poll_ms": 100,
  "publish_interval_s": 0.5,
  "bring_up_can": true,
  "can_bitrate": 500000,
  "output": "/dev/shm/gocontroll/multibus-cells.json"
}
```

| Key | Meaning |
|-----|---------|
| `slot` | Controller slot the module sits in, 1..8. |
| `interfaces` | Five modes, one per connector interface. Which are allowed is in [multibus-interfaces.md](multibus-interfaces.md). |
| `cell_count` | How many cells the battery side reports, 1..24. |
| `module_poll_ms` | How often the module polls the battery over isoSPI. |
| `publish_interval_s` | How often the service reads the module and rewrites the file. |
| `bring_up_can` | Whether the service raises the module's CAN interfaces. |
| `can_bitrate` | Fallback bit rate, used only when go-can cannot configure them. |
| `output` | Where the cell data is published. |

`module_poll_ms` and `publish_interval_s` are independent. The module keeps its
own most recent snapshot, so a publish never waits for a battery round trip - it
always gets an answer immediately, with an `age_ms` field saying how old that
answer is.

### Device names

The kernel names USB devices in discovery order, which depends on the
controller. A Moduline IV has four onboard mcp251x CAN controllers, so the
module's channels land on `can4`..`can6`; an M1 has two, so the same module
lands on `can2`..`can4`. The `ttyACM` numbers move for the same reason.

`debian/udev/81-gocontroll-multibus.rules` pins them down:

| Name               | Knowledge base    | Connector interface | Pins  |
|--------------------|-------------------|---------------------|-------|
| `mb_can1`          | CAN 1             | 1                   | 6/5   |
| `mb_can2`          | CAN 2             | 3                   | 18/19 |
| `mb_can3`          | CAN 3             | 4                   | 24/25 |
| `/dev/mb_serial1`  | RS485 1 / RS232 1 | 2                   | 12/13 |
| `/dev/mb_serial2`  | RS485 2           | 3                   | 18/19 |
| `/dev/mb_serial3`  | RS485 3           | 4                   | 24/25 |
| `/dev/mb_protocol` | -                 | 5 (isoSPI) + control | -    |

The names follow the [knowledge base][kb] numbering: CAN 1..3 and RS485 1..3.

**That is not the same as the connector interface numbering** used in
`MbInterfaces.c` and in the `interfaces` configuration array. `mb_can2` is
connector interface 3. The two schemes exist because the buses are numbered per
type while the connector is numbered per pin pair, and interface 2 has no CAN.

`mb_can2` and `/dev/mb_serial2` are the same connector pins in different modes,
as are `mb_can3` and `/dev/mb_serial3`. Only one of each pair can be live.

The CAN interfaces are renamed; the serial ports get symlinks alongside their
`ttyACM` nodes, so `cdc_acm` keeps the device it created.

[kb]: https://gocontroll.com/knowledge-base/modular-hardware/modules/multibus/

Install the rule with:

```bash
install -m 644 debian/udev/81-gocontroll-multibus.rules /etc/udev/rules.d/
install -m 755 debian/udev/gocontroll-multibus-canname /lib/udev/
udevadm control --reload-rules && udevadm trigger --action=add
```

Without the rule the tooling still works - it falls back to discovering the
interfaces through sysfs - but anything else on the controller that refers to a
name has to do the same. With more than one Multibus module present the names
collide and only the first module gets them; that case needs a rule keyed on
the serial number.

### Which CAN interfaces come up

The module always presents three gs_usb channels regardless of configuration,
because the USB descriptor is fixed. The service only brings up the ones with a
live transceiver: `mb_can1` is always CAN, `mb_can2` and `mb_can3` follow
connector interfaces 3 and 4. A channel whose interface is RS485 is left down,
since a netdev that can never carry a frame is worse than no netdev at all.

**The bitrate is go-can's, not ours.** Each interface is raised with
`go-can apply <iface>`, which reads `/etc/gocontroll/can.d/<iface>.conf`. Setting
a bitrate here as well would make the two tools disagree - go-can would keep
reporting its stored value while the interface actually ran at the service's.
`can_bitrate` in the service config is only a fallback for a controller where
go-can is not installed, or has no config for the interface yet.

## Bitrates: go-can, not go-multibus

`go-multibus` decides *what* an interface is - CAN or RS485 - because only the
module can switch a transceiver. Once an interface is CAN it is an ordinary
SocketCAN device, so its bitrate and the rest of its parameters belong to
`go-can`, the same as for the baseboard buses:

```console
# go-can list
NAME      PRESENT  UP       CONFIGURED  CONFIG
can0      yes      yes      yes         /etc/gocontroll/can.d/can0.conf
...
mb_can1   yes      yes      yes         /etc/gocontroll/can.d/mb_can1.conf
mb_can2   yes      yes      yes         /etc/gocontroll/can.d/mb_can2.conf
mb_can3   yes      no       no          /etc/gocontroll/can.d/mb_can3.conf

# go-can set mb_can1 bitrate 500000
# go-can show mb_can1
# go-can            # the same interactive TUI, now including the module buses
```

That split is worth keeping straight: **go-multibus owns the mode, go-can owns
the bus parameters.** Setting a bitrate on an interface currently configured as
RS485 will succeed and do nothing useful, because the transceiver is in standby.

### What had to change in go-can

`list_can_devices()` matched `canN` only, so the renamed module interfaces were
invisible to both `go-can list` and its TUI. It now matches `mb_canN` as well.

More subtly, the module's buses needed their own defaults. gs_usb supports fewer
netlink options than the baseboard's mcp251x controllers, and two of the
baseboard defaults are rejected outright:

| Option            | gs_usb response                                  |
|-------------------|--------------------------------------------------|
| `triple-sampling` | `RTNETLINK answers: Operation not supported`     |
| `restart-ms 100`  | `Device doesn't support restart from Bus Off`    |

Without that change the very first `go-can set mb_can1 bitrate 500000` saved the
config and then failed to apply it. `defaults::default_for_module()` turns both
off, and `config::load_or_default()` seeds new configs from it.

**Bus-off recovery is therefore the application's problem on these interfaces.**
The driver will not restart the controller by itself the way the baseboard ones
do.

## What it publishes

`/dev/shm/gocontroll/multibus-cells.json` holds the measurements; the format is
described in [gocontroll-cellmon.md](gocontroll-cellmon.md#the-published-document).

`/dev/shm/gocontroll/multibus-state.json` holds where the bring-up sequence
stands:

```json
{
  "schema": 1,
  "timestamp": 1755340800.123,
  "state": "running",
  "slot": 8,
  "module_running": true,
  "interfaces": ["can", "rs485", "can", "rs485", "isospi"],
  "failures": 0,
  "last_error": ""
}
```

Both are written to a temporary file and moved into place with `os.replace`,
which is atomic on Linux. A reader either sees the previous document or the new
one in full, never half of either, so it needs no locking and no cooperation
with the writer.

Failures are published as well, rather than leaving the last good document in
place. A reader that only ever sees a stale file cannot tell a dead service from
a dead battery link; this way it can.

## Troubleshooting

| `state` | What it means |
|---------|---------------|
| stuck in `start` | The module does not answer on the module bus. Either the slot is empty or the reset line does not reach the STM32. |
| stuck in `configure` | The bootloader handed over but the application does not answer. Most likely an older build without `ModuleBus.c`; apply the configuration over USB instead. |
| stuck in `enumerate` | The application runs but USB does not come up. The log says whether the controller's OTG port booted in the wrong role; if it did, `gocontroll-usb-hostmode --apply` and a reboot fix it. Otherwise look at the cable and the module. |
| `running`, `failures` climbing | USB works but polls fail. `journalctl -u go-multibus` has the error text. |
| `running` but `valid` false | USB is fine and the isoSPI link is not. Use `gocontroll-cellmon status` on the counters. |

## Related documents

- [multibus-interfaces.md](multibus-interfaces.md) - the connector, which
  combinations exist, and why the RS485 labels do not match the interface numbers.
- [multibus-usb-protocol.md](multibus-usb-protocol.md) - frame format and command
  reference for the USB side.
- [gocontroll-cellmon.md](gocontroll-cellmon.md) - the cell data itself and the
  standalone tooling for reading it.
