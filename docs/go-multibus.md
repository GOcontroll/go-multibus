# go-multibus: the Multibus service on the controller

`go-multibus` brings a Multibus module up after a controller boot, configures
its interfaces, and keeps the battery cell data flowing to the rest of the
system. It is the piece that makes the module work without anyone typing
anything - and with more than one module in the controller, it does that for
each of them.

> The file names in this document - `MbInterfaces.c`, `MbSupply.c`,
> `MbProto.c`, `usbd_composite.c` - are firmware sources and live in the
> [Module-Multibus](https://github.com/GOcontroll/Module-Multibus)
> repository, not here. That repository is also where these documents are
> edited; the copies here are installed by the package.

It has two faces over one state machine: a background service that runs at boot,
and a terminal UI for commissioning a machine. What the UI shows is exactly what
the service does, because both drive the same code.

It lives in its own repository,
[GOcontroll/go-multibus](https://github.com/GOcontroll/go-multibus), and is a
Rust binary built on `crossterm` - the same as `go-can` and `go-modules`, which
is what makes the three tools look and behave alike rather than merely similar.

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
- The external supply on the connector has to be what the machine expects. The
  module remembers that one by itself, but the controller still owns the value -
  see [multibus-external-supply.md](multibus-external-supply.md).
- Two modules on one isoSPI link have to be one master and one slave, and the
  module does not remember which it is. Every module of a hardware revision
  gets the same image, so the role cannot come from the build either.

The order matters, and it is the reason the configuration goes over SPI:

```
   reset ──► start ──► configure ──► enumerate ──► running
             (SPI)      (SPI)          (USB)        (USB)
                          ▲                ▲
                          │                └─ by now the controller knows which
                          │                   canX and ttyACMx are meaningful
                          └─ has to happen before USB comes up
```

## The states

| State       | What happens                                                        |
|-------------|---------------------------------------------------------------------|
| `reset`     | Pulse the slot reset line. The STM32 restarts into the bootloader.   |
| `start`     | Identify the module, then send the bootloader `UP_TO_DATE` so it jumps to the application. |
| `configure` | Send `IF_SET` over the module bus with the five interface modes, then `ROLE_SET` with the isoSPI role, `SUPPLY_SET` with the external supply, and - with more than one module configured - `IDENTITY` for the USB serial number. |
| `enumerate` | Wait for the USB device and its four CDC ports, then open the protocol port and apply the cell configuration. With several modules: wait for *this* module's USB device, found by its serial number, check its names, and confirm over USB that it is the right one. |
| `running`   | Poll the cell snapshot and publish it.                               |

Every step is idempotent, and the sequence is re-entered from the top whenever
the module disappears. That is what makes recovery automatic: pull the module
and put it back, and the service resets, restarts, reconfigures and resumes
without anyone intervening.

One more state sits outside the sequence: **`held`**. go-modules (3.3.2 and
later) writes its process id to `/run/gocontroll/module-busy/<slot>` while it
scans or flashes a slot. A supervisor that finds its slot marked closes the
protocol port, touches nothing, and waits; when the marker goes - or the process
that wrote it is gone - it brings the module back from `reset`, because go-modules
leaves it in its bootloader or in an application nobody has configured. Before this a firmware update fought
the service: it saw its module vanish halfway through the upload and reset the
slot, which killed the upload. The other modules keep running: `go-modules
update 8` only touches slot 8.

With more than one module configured each runs this sequence in its own thread,
and every line in the journal starts with `slot N: `. A module that is being
reset never holds up another one's data.

A failure inside `running` gets a three second grace period before the module is
reset, because a single dropped USB poll is not a reason to restart a working
module. A failure during bring-up restarts the sequence immediately.

## Interfaces over SPI, cell data over USB

Two links to the same module, each doing what it is good at:

**The module bus (SPI)** exists before USB and stays available regardless of
whether USB enumerated. It carries the interface configuration, the isoSPI
role, the external supply, the module's identity and a compact status message.
`ModuleBus.c` serves it in the application, using the same 46 byte message
format as the bootloader so `gocontroll-modulebus` needs no second code path.

It is also the one link that is tied to a slot - each slot has its own chip
select - which is what makes it the place to ask a module for its USB serial
number when there are several. USB reaches devices, not slots.

Because SPI is full duplex, the module cannot answer in the transfer that
carries the request - it only learns what was asked once the last byte is
clocked in. The reply goes out with the *next* transfer, so the controller
sends the same message repeatedly until the matching answer comes back. That is
safe because every application message is idempotent.

**USB** carries the cell data, which is far too large and too frequent for a 46
byte message. See [multibus-usb-protocol.md](multibus-usb-protocol.md).

## Running it

### As a service

The controller tooling is packaged, so on a board on the GOcontroll apt
repository this is the whole story:

```bash
apt-get install go-multibus
systemctl enable --now go-multibus
```

From a checkout of the go-multibus repository, build and install by hand:

```bash
cargo build --release       # on the controller itself
sudo ./install.sh           # same paths the package uses
```

or cross-build and point `BINDIR` at the result:

```bash
cargo zigbuild --release --target aarch64-unknown-linux-musl
BINDIR=target/aarch64-unknown-linux-musl/release ./install.sh
```

`install.sh` also stages into a `PREFIX` for the package build, so the package
and a manual install cannot drift apart.

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
  GOcontroll Moduline Multibus tool  v0.3.0
  ------------------------------------
  Module: slot 8   running
  ------------------------------------
    Interface 1   6/5      CAN FD      fixed
    Interface 2   12/13    RS485
  > Interface 3   18/19    CAN FD
    Interface 4   24/25    RS485
    Interface 5   isoSPI   isoSPI
    isoSPI role            master
    Supply        5-15 V   9 V         kept over a reset
  ------------------------------------
  up/down navigate   Enter select   c cells   r restart   q quit
```

With more than one module configured the header says which one the list is
about, and Tab moves on to the next:

```
  Module: slot 7   running   Tab next module
```

Every module has its own list, its own log lines underneath, and its own cell
view; all of them are looked after while the UI is open, not just the one on
screen.

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

Interface 1 is fixed, so its picker has a single entry and a note saying it is
not configurable.

The **isoSPI role** row picks which end of the battery link the module drives.
It applies without a reset: the module switches on the spot, and the role is
stored in the configuration, because the module itself forgets it at the next
reset and the service pushes it again on every bring-up.

```
  isoSPI role  -  Select Role

  > master
    slave
  ------------------------------------
  up/down navigate   Enter apply   left/Esc back
```

A one-module configuration that never set a role shows `default` and leaves
the module in whatever role its image starts in, with `module runs as master`
beside it. A module running in a different role than the one configured says
`module has slave` in red - and adds `fixed by its firmware` when it runs an
image older than 0.3.0, which cannot switch.

The last row is the **external supply**, which also applies without a reset -
the module takes the setpoint immediately and writes it to its own flash:

```
  External supply  -  Select Voltage

    off
    5 V
    6 V
  > 7 V
    ...
    15 V
  ------------------------------------
  up/down navigate   Enter apply   left/Esc back
```

One picker covers both the on/off state and the voltage: `off` disables the
regulator, any voltage enables it at that setpoint. The steps come from what
the module reports, not from a table in the tool.

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
supply     : 9 V
cells      : 12, pack 43.464 V, spread 0.0440 V (simulated)

$ go-multibus config
$ go-multibus set 3 can
$ go-multibus supply 9V
$ go-multibus role slave
$ go-multibus ports
```

`set` writes the configuration file and tells you to restart the service; it
does not touch a running module, so a scripted change and a running machine
never disagree about what is happening.

`supply` and `role` are the exceptions, and for the same reason the UI's
pickers are: neither needs a reset, so each is stored *and* applied straight
away when the module is reachable. When it is not, the value is still stored
and the next bring-up pushes it. `role` without an argument shows the
configured role next to the one the module reports.

`status --json` prints the two published documents instead of the summary.

With more than one module, `status` shows each of them in turn - with its role
and USB serial number - and `status --slot 7` only that one; `--json` then
prints a `modules` list. `set`, `supply` and `role` need `--slot` to say which
module they are about:

```console
$ go-multibus set --slot 7 3 can
$ go-multibus role --slot 7 slave
$ go-multibus supply --slot 8 off
```

`ports` lists every module present: its USB serial number, the slot the name
map puts it in, its serial ports and protocol port under their udev names, and
its CAN interfaces.

`module add` and `module remove` edit the list of modules:

```console
$ go-multibus module add 7
slot 7 added to /etc/gocontroll/multibus.json; configured slots: 7, 8
the file now holds a module list
slot 8 had no isospi_role and is now master, the default in a module list; ...
slot 7 is now named mb7_can1..3, /dev/mb7_serial1..3 and /dev/mb7_protocol, ...
slot 8 is now named mb8_can1..3, /dev/mb8_serial1..3 and /dev/mb8_protocol, ...
restart the service for it to take effect: systemctl restart go-multibus
```

Adding a second module turns a one-module file into a module list and says
what changes for the module that was already there - its names and its
published file now carry its slot. Removing down to one module keeps the list
with one entry, and that module goes back to the plain `mb_` names.

### The lower-level tools

`gocontroll-cellmon` talks to one module's protocol port. With several present,
`--slot 7` picks the module the name map puts in slot 7 and `--serial` the one
with that USB serial number; `list` shows both for every module.

```console
$ gocontroll-cellmon list
$ gocontroll-cellmon --slot 7 identity        # USB serial number and chip UID
$ gocontroll-cellmon --slot 7 role slave      # until the next reset
$ gocontroll-cellmon --slot 8 link-test --count 1000 --min 1 --max 80
```

`link-test` bounces counting byte patterns off the slave through the master's
`LINK_TEST`, cycling the length from `--min` to `--max`, and prints how many
came back ok, with no answer, with a CRC error or changed, plus the minimum,
mean and maximum round trip as the master timed it. Run it on the master; a
slave answers that it cannot.

`gocontroll-modulebus` has `role` and `identity` too, over SPI, which work
before USB is up and say which slot a serial number belongs to:

```console
$ gocontroll-modulebus 7 identity
$ gocontroll-modulebus 7 role master
```

A role set with either tool lasts until the module is next reset. Only the
configuration survives that, which is what `go-multibus role` writes.

## Before any of this works: the OTG port

A Multibus module hangs off the controller's OTG USB port, and on an i.MX8MM
Moduline that port comes up as a USB *device* (the serial console gadget) unless
it has been told otherwise. A port in that role can never enumerate a module,
however often the module is reset - and the symptom gives nothing away: the
module simply never appears.

This is a once-per-controller change, and it survives reboots:

```console
# gocontroll-usb-hostmode            # report what the port booted as
# gocontroll-usb-hostmode --apply    # switch it to host
# reboot
```

Check afterwards:

```bash
tr -d '\0' < /proc/device-tree/soc@0/bus@32c00000/usb@32e40000/dr_mode   # -> host
dmesg | grep 'new .*-speed USB device'
```

The setting lives in the u-boot environment (`otg_mode`), not in the device
tree, so it only takes effect on the boot *after* the one that writes it. The
`go-multibus` package applies it during `configure` for exactly that reason:
left to the service, the first reboot after installing would still come up in
peripheral mode and a second would be needed before anything enumerated.

**`dr_mode=otg` with a runtime role switch is not good enough.** That leaves the
i.MX8 PHY half initialised: the host sees a device but the module never sees a
bus reset, so enumeration fails with `device descriptor read/64, error -71` and
no amount of firmware work will fix it. `host` is the value that works.

`go-multibus` names this outright when a bring-up times out, rather than saying
"check the cable": it reads the device tree node and, if the port is in the
wrong role, says so and what to run.

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
  "output": "/dev/shm/gocontroll/multibus-cells.json",
  "supply_enabled": false,
  "supply_mv": 5000
}
```

| Key | Meaning |
|-----|---------|
| `slot` | Controller slot the module sits in, 1..8. |
| `interfaces` | Five modes, one per connector interface. Which are allowed is in [multibus-interfaces.md](multibus-interfaces.md). |
| `cell_count` | How many cells the battery side reports, 1..24. |
| `module_poll_ms` | How often the module polls the battery over isoSPI. |
| `publish_interval_s` | How often the service reads the module and rewrites the file. |
| `bring_up_can` | Whether the service runs `ip link set canX up`. |
| `can_bitrate` | Bit rate for those interfaces. |
| `output` | Where the cell data is published. |
| `supply_enabled` | Whether the module's regulator is on. |
| `supply_mv` | Its setpoint in mV, a whole number of volts from 5000 to 15000. |
| `isospi_role` | `master` or `slave`, pushed on every bring-up. Left out of a one-module file, the module keeps the role its image starts in. |

`module_poll_ms` and `publish_interval_s` are independent. The module keeps its
own most recent snapshot, so a publish never waits for a battery round trip - it
always gets an answer immediately, with an `age_ms` field saying how old that
answer is.

### More than one module

A controller with several modules lists them under `modules`. Every entry takes
the keys of the table above except `publish_interval_s`, which stays at the top
because there is one publish rhythm per service; only `slot` is required, and
`isospi_role` defaults to `master`.

```json
{
  "publish_interval_s": 0.5,
  "modules": [
    {
      "slot": 7,
      "isospi_role": "slave",
      "interfaces": ["can", "rs485", "can", "rs485", "isospi"],
      "cell_count": 12,
      "supply_enabled": false
    },
    {
      "slot": 8,
      "isospi_role": "master",
      "supply_enabled": true,
      "supply_mv": 9000
    }
  ]
}
```

Slots are 1..8 and each may appear once; a file with a slot twice is refused
rather than half used. `output` may be set per module, and two modules may not
share one.

What decides the behaviour is the number of modules, not the form of the file:
a list with one entry is named and published exactly like the one-module form,
`mb_can1` and `multibus-cells.json` included. That is what `module remove`
leaves behind, and it means going from two modules back to one never needs the
file rewritten by hand.

A file is written back in the form it was read in. A one-module file that has
had an interface changed comes back exactly as an older release would have
written it - `isospi_role` only appears once a role has been set.

### Device names

The kernel names USB devices in discovery order, which depends on the
controller. A Moduline IV has four onboard mcp251x CAN controllers, so the
module's channels land on `can4`..`can6`; an M1 has two, so the same module
lands on `can2`..`can4`. The `ttyACM` numbers move for the same reason.

`debian/udev/81-gocontroll-multibus.rules`, installed by the package, pins them
down:

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
name has to do the same.

### Device names with more than one module

Two modules would both claim `mb_can1`, and udev would leave the second on its
kernel name. So once more than one module is configured, every name carries
the slot:

| Slot 7              | Slot 8              | Knowledge base    |
|---------------------|---------------------|-------------------|
| `mb7_can1..3`       | `mb8_can1..3`       | CAN 1..3          |
| `/dev/mb7_serial1..3` | `/dev/mb8_serial1..3` | RS485 1..3 / RS232 1 |
| `/dev/mb7_protocol` | `/dev/mb8_protocol` | -                 |

The longest, `mb8_can3`, fits easily in the 15 characters a network interface
name may have.

The difficulty is that nothing in a uevent says which slot a device is in: both
modules have the same vendor and product id and the same interfaces. The USB
serial number tells them apart but says nothing about slots. The module bus
closes the gap - it reaches one slot, and from 0.3.0 the module answers
`IDENTITY` on it with the very serial number its USB descriptor carries. The
service asks during `configure` and writes down what it learned, one module per
line:

```
206435A63245 mb7_
2064359B3245 mb8_
```

`/run/gocontroll/multibus-usb-names` is the live copy, rewritten as modules come
up. `/var/lib/go-multibus/usb-names` survives a reboot, so a module that
enumerates before the service has run is still named right. The udev helper
looks each device's serial number up, live copy first, and puts the prefix in
front of the name; `gocontroll-multibus-canname --prefix <serial>` prints just
the prefix, which is what the tty rules use.

USB enumerates at about the same moment the service learns the serial number,
so the two race:

- A map that does not list the serial yet makes the helper wait up to three
  seconds for the line to appear. The service creates the live map before it
  resets the first module, which is what makes that wait happen even on the
  very first boot.
- Whatever udev still named first - with the `mb_` names, or its kernel name -
  is put right by the service: after enumeration it looks at *its* module's
  devices only (found by serial number, not by name), and if any is wrong has
  udev process exactly those again with `udevadm trigger --action=add`. The CAN
  interfaces are still down at that point, so the rename succeeds. Another
  module's devices are never touched.

With one module configured there is no map: the service removes one left over
from a configuration with more, and the helper then gives every Multibus device
the `mb_` names, exactly as before. A module running firmware older than 0.3.0
has no `IDENTITY` and cannot be told apart from another one, so with more than
one module configured its bring-up fails with
`firmware older than 0.3.0 cannot be told apart from another module; update it`.

`gocontroll-cellmon --slot 7` finds a module through the same map, and
`--serial` takes the serial number directly.

### Which CAN interfaces come up

The module always presents three gs_usb channels regardless of configuration,
because the USB descriptor is fixed. The service only brings up the ones with a
live transceiver: `mb_can1` is always CAN, `mb_can2` and `mb_can3` follow
connector interfaces 3 and 4. A channel whose interface is RS485 is left down,
since a netdev that can never carry a frame is worse than no netdev at all.

With several modules each service thread only looks at its own module's
netdevs, found under its USB device, and raises them by their slot names
(`mb7_can1`). One that udev has not renamed is left alone rather than raised
under the wrong name, because an interface that is up can no longer be renamed.

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
invisible to both `go-can list` and its TUI. It now matches `mb_canN` as well,
and `mb<slot>_canN` - `mb7_can1` - for a controller with more than one module.

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

The driver cannot restart a controller after bus-off, so since firmware 0.3.0
**the module does it itself**, 100 ms after the bus-off - the same recovery the
baseboard buses get from `restart-ms 100`. The kernel still sees the bus-off and
the restart as error frames, so `ip -details -statistics link show` counts both.

### CAN FD

All three module buses are CAN FD capable, and from firmware 0.3.0 on the kernel
knows it, so the usual go-can keys apply:

```console
# go-can set mb_can1 bitrate 500000
# go-can set mb_can1 data-bitrate 2000000
# go-can set mb_can1 fd on
```

Set the data bitrate before switching FD on: `fd on` without one is refused by
the kernel. On firmware older than 0.3.0 `fd on` fails with `Operation not
supported`. `ip -details link show` reports the error counters as
`berr-counter tx N rx N`, read live from the module.

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
  "supply": {
    "enabled": true, "volts": 9.0, "millivolts": 9000,
    "min_mv": 5000, "max_mv": 15000, "step_mv": 1000, "stored": true
  },
  "failures": 0,
  "last_error": ""
}
```

`supply` is `null` on a module whose firmware predates 0.2.0.

With more than one module configured each publishes its own pair,
`multibus-slot7-cells.json` and `multibus-slot7-state.json` (unless its entry
sets `output`), and the state document gains two fields:

```json
{
  "slot": 7,
  "isospi_role": { "configured": "slave", "reported": "slave" },
  "usb_serial": "206435A63245",
  ...
}
```

`reported` is what the module says it runs as, `null` until it has been asked.
With one module the documents keep their names and their shape.

Both are written to a temporary file and moved into place with `rename(2)`,
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
| stuck in `enumerate` | The application runs but USB does not come up. Check that the controller port is in host mode. |
| `running`, `failures` climbing | USB works but polls fail. `journalctl -u go-multibus` has the error text. |
| `running` but `valid` false | USB is fine and the isoSPI link is not. Use `gocontroll-cellmon status` on the counters. |
| `held` | go-modules is scanning or flashing this slot. It clears on its own when go-modules exits; a marker in `/run/gocontroll/module-busy/` whose process is gone is ignored. |
| supply row says `not supported by this firmware` | The module runs a build older than 0.2.0. Everything else works; flash a current image to get the supply. |
| `firmware older than 0.3.0 cannot be told apart from another module` | More than one module is configured and this one has no `IDENTITY`. Flash 0.3.0 or newer. |
| role row says `module has ...` | The module runs in another isoSPI role than configured. `fixed by its firmware` means an image older than 0.3.0, which cannot switch. |
| a module keeps `mb_` or kernel names next to another module | Check `/run/gocontroll/multibus-usb-names` lists its serial (`gocontroll-cellmon list` shows it) and that the udev rule is installed. The service logs which devices it could not rename. |

## Related documents

- [multibus-interfaces.md](multibus-interfaces.md) - the connector, which
  combinations exist, and why the RS485 labels do not match the interface numbers.
- [multibus-usb-protocol.md](multibus-usb-protocol.md) - frame format and command
  reference for the USB side.
- [multibus-external-supply.md](multibus-external-supply.md) - the regulator on
  the connector, its range, and why it is the one setting the module remembers.
- [gocontroll-cellmon.md](gocontroll-cellmon.md) - the cell data itself and the
  standalone tooling for reading it.
