# go-multibus

GOcontroll Multibus module service and utilities for Moduline controllers
(L4, M1, HMI1) running on i.MX8MM hardware.

Brings a Multibus module up after a controller boot, configures which physical
interface each connector pin pair carries, sets the external supply on the
connector and the module's end of the isoSPI link, and publishes the battery
cell voltages it reads over isoSPI to `/dev/shm` for the rest of the system.
A controller can carry more than one module; each is brought up on its own.

Two faces over one state machine: a background service that runs at boot, and a
terminal UI for commissioning a machine. What the UI shows is what the service
does, because both drive the same code.

Rust and `crossterm`, the same as `go-can` and `go-modules` - which is what
makes the three tools look and behave alike rather than merely similar. The
result is three static binaries and no runtime dependencies on the controller.

## CLI

```sh
gocontroll-usb-hostmode                  # report the OTG port's mode
gocontroll-usb-hostmode --apply          # put it in host mode (needs a reboot)

go-multibus                              # interactive UI (no arguments)
go-multibus daemon                       # background service, systemd entry
go-multibus status                       # one-shot summary of every module
go-multibus config                       # show the stored configuration
go-multibus set 3 can                    # change one interface, store it
go-multibus supply 9V                    # external supply: store and apply
go-multibus supply off
go-multibus role slave                   # isoSPI role: store and apply
go-multibus module add 7                 # configure a second module
go-multibus module remove 7
go-multibus ports                        # modules, ports and CAN interfaces

gocontroll-cellmon list                  # which Multibus modules are present
gocontroll-cellmon info                  # identity, role, capabilities
gocontroll-cellmon identity              # USB serial number and chip UID
gocontroll-cellmon status                # isoSPI link counters
gocontroll-cellmon cells                 # one cell snapshot
gocontroll-cellmon watch                 # keep printing snapshots
gocontroll-cellmon interfaces --set can,rs485,can,rs485,isospi
gocontroll-cellmon supply 9V             # external supply, over USB
gocontroll-cellmon role slave            # isoSPI role, until the next reset
gocontroll-cellmon link-test --count 500 --min 1 --max 80
gocontroll-cellmon raw 03 60 f4 6c       # raw isoSPI transfer
gocontroll-cellmon serve                 # publish to /dev/shm

gocontroll-modulebus 8 probe             # module bus handshake, over SPI
gocontroll-modulebus 8 start             # bootloader -> application
gocontroll-modulebus 8 interfaces        # read/write config over SPI
gocontroll-modulebus 8 supply 9V         # external supply, before USB exists
gocontroll-modulebus 8 role              # isoSPI role, over SPI
gocontroll-modulebus 8 identity          # USB serial number, over SPI
```

With more than one module configured, `set`, `supply` and `role` need
`--slot N` to say which module they are about (`go-multibus set --slot 7 3 can`);
with one it may be left out. `status --slot N` shows one module. For
`gocontroll-cellmon`, `--slot N` or `--serial <usb-serial>` picks the module
when several are present.

Only one of `go-multibus daemon` and the UI can run at a time: both open the
module's protocol port, and two processes on one CDC port read each other's
answers. The UI refuses to start while the service is active and says so.

## The bring-up sequence

```
   reset ──► start ──► configure ──► enumerate ──► running
             (SPI)      (SPI)          (USB)        (USB)
                          ▲                ▲
                          │                └─ the controller now knows which
                          │                   CAN and serial ports are real
                          └─ has to happen before USB comes up
```

| State       | What happens                                                    |
|-------------|-----------------------------------------------------------------|
| `reset`     | Pulse the slot reset line; the STM32 restarts into the bootloader |
| `start`     | Tell the bootloader its firmware is current so it starts the application |
| `configure` | Push the five interface modes, the isoSPI role and the external supply over the module bus, and ask the module for its USB serial number |
| `enumerate` | Wait for USB, open the protocol port, apply the cell configuration |
| `running`   | Poll the cell snapshot and publish it                            |

Every step is idempotent and the sequence restarts from the top when the module
disappears, so pulling a module and putting it back recovers on its own. With
several modules each runs this sequence in its own thread, so one module being
reset never holds up another one's data.

The configuration goes over SPI rather than USB because the controller has to
know the interface layout *before* it initialises the interfaces, and USB is not
up at that point. When the module bus does not answer - firmware older than
`ModuleBus.c` - the service falls back to configuring over USB.

## Interfaces

The connector carries five interfaces. Pin numbers are for the 26 position
uneven slot.

| Interface | Pins  | Modes           | Linux                          |
|-----------|-------|-----------------|--------------------------------|
| 1         | 6/5   | CAN FD, fixed   | `mb_can1`                      |
| 2         | 12/13 | RS485 or RS232  | `/dev/mb_serial1`              |
| 3         | 18/19 | RS485 or CAN FD | `mb_can2` / `/dev/mb_serial2`  |
| 4         | 24/25 | RS485 or CAN FD | `mb_can3` / `/dev/mb_serial3`  |
| 5         | -     | isoSPI, fixed   | `/dev/mb_protocol`             |

At most three CAN channels and at most three serial ports can be live at once.
`mb_can2` and `/dev/mb_serial2` are the same connector pins in different modes;
only one of each pair carries traffic.

Bus parameters - bitrate, sample point, FD - belong to `go-can`, the same as for
the baseboard buses. **go-multibus owns the mode, go-can owns the parameters.**
The service raises each interface with `go-can apply`, so the two never disagree
about what a bus is running at.

## The external supply

The module can feed whatever is plugged into the connector from its own
regulator, at 5 V to 15 V in whole volts. `go-multibus` shows it as the last row
of its list, and Enter opens a picker with `off` and every volt the module
accepts - the steps come from what the module reports, not from a table here.

**It is the one setting the module remembers by itself.** The firmware keeps it
in flash and restores it before anything else starts, so a sensor powered by the
module gets its voltage back after a reset without waiting for Linux. The
controller's copy still wins: `supply_enabled` and `supply_mv` are pushed on
every bring-up, so an edit made while the module was out of its slot takes
effect when it comes back.

Changing it needs no reset, unlike an interface change. The module takes the
setpoint immediately, which is why `go-multibus supply` and the UI's picker both
store *and* apply in one step.

There is no feedback path from the regulator back into the MCU, so everything
reported is the setpoint, never a measurement. Details in
[docs/multibus-external-supply.md](docs/multibus-external-supply.md).

## The isoSPI role

Two modules on one isoSPI link have to be one master and one slave. The role
used to be baked into the firmware image, but go-modules gives every module of
a hardware revision the same image, so two of them would both come up as
masters. From firmware 0.3.0 the controller decides: `isospi_role` in the
configuration, pushed over the module bus on every bring-up, like the
interfaces.

**The module does not remember it.** After a reset it is back in the role its
image starts in until the controller says otherwise, which is why the
configuration file is what carries the role. Changing it needs no reset, so
`go-multibus role slave` and the UI's role row store *and* apply it in one step.

A configuration with one module that never set `isospi_role` pushes nothing and
leaves the module in its image's role, exactly as before 0.3.0. In a module list
the default is `master`.

## Device names

Names are pinned by `debian/udev/81-gocontroll-multibus.rules`, because kernel
names depend on the controller: a Moduline IV has four onboard mcp251x
controllers so the module lands on `can4`..`can6`, while an M1 has two and the
same module lands on `can2`..`can4`. The numbering follows the knowledge base
(CAN 1..3, RS485 1..3), not the connector interface numbers.

The tooling falls back to sysfs discovery when the rule is not installed, but
anything else on the controller that refers to a name needs it.

### More than one module

Two modules would both claim `mb_can1`, so with more than one module configured
every name carries the slot instead:

| Slot 7              | Slot 8              |
|---------------------|---------------------|
| `mb7_can1..3`       | `mb8_can1..3`       |
| `/dev/mb7_serial1..3` | `/dev/mb8_serial1..3` |
| `/dev/mb7_protocol` | `/dev/mb8_protocol` |

Nothing in a uevent says which slot a USB device sits in - both modules have
the same ids and the same interfaces. What tells them apart is the USB serial
number, and the module bus, which does reach one particular slot, is where the
service asks for it (firmware 0.3.0). It writes the answer down, one line per
module:

```
206435A63245 mb7_
2064359B3245 mb8_
```

in `/run/gocontroll/multibus-usb-names`, with a copy in
`/var/lib/go-multibus/usb-names` that survives a reboot. The udev helper looks
every device's serial number up there and puts the prefix in front of the name.
A device udev reached before the line was written is named again by the service
once it is, while its CAN interfaces are still down.

One module - a plain configuration, or a module list with one entry - has no map
and keeps the `mb_` names, so nothing about a single-module controller changes.

## Config file

`/etc/gocontroll/multibus.json`. A missing file is a working configuration.

One module, the form every release has read:

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

More than one module, as a list. Every entry takes the same keys as the
one-module form plus `isospi_role`, and only `slot` is required; the rest
default as above. `publish_interval_s` stays at the top.

```json
{
  "publish_interval_s": 0.5,
  "modules": [
    { "slot": 7, "isospi_role": "slave", "interfaces": ["can", "rs485", "can", "rs485", "isospi"] },
    { "slot": 8, "isospi_role": "master", "supply_enabled": true, "supply_mv": 9000 }
  ]
}
```

Slots must be 1..8 and different. `go-multibus module add 7` turns a one-module
file into a list and `module remove` takes an entry out again; a list with one
entry behaves exactly like the one-module form.

`interfaces` is indexed by **connector interface**, so `interfaces[2]` is
interface 3 and drives `mb_can2`.

## Published documents

`/dev/shm/gocontroll/multibus-cells.json` holds the measurements and
`multibus-state.json` where the bring-up sequence stands. Both are written to a
temporary file and moved into place with `rename(2)`, which is atomic on Linux:
a reader sees either the previous document or the new one, never half of either,
and needs no locking.

With more than one module each gets its own pair,
`multibus-slot7-cells.json` and `multibus-slot7-state.json`, unless its entry
sets `output`. The state document then also carries `isospi_role` (configured
and reported) and `usb_serial`.

Branch on `valid` - it is `link_ok && !stale`. Failures are published too, rather
than leaving the last good document in place, so a reader can tell a dead service
from a dead battery link.

Voltages appear twice on purpose: `cells_v` rounded for display, `cells_100uv`
as the raw device values in the 100 uV steps the LTC681x family uses.

## Systemd integration

```sh
systemctl enable --now go-multibus       # service + cell publisher
```

`gocontroll-cellmon.service` is an alternative for a controller that only wants
the cell data and configures the module some other way. Do not run both.

## Installing

```sh
apt-get install go-multibus
```

That is everything a fresh controller needs: the tools, the service, the udev
naming rules, and `gocontroll-usb-hostmode` for the OTG port. Both services are
enabled but not started - at install time there may be no controller under the
filesystem, and on real hardware starting the service resets the module and
raises CAN interfaces, which an install should not do underneath a running
machine.

```sh
systemctl start go-multibus
```

**A controller whose OTG port is still in device mode needs one reboot.** The
install switches the port itself, but the change lands in the u-boot environment
and the Falcon boot blob, so it only takes effect on the next boot. Until then
no module enumerates. On a controller already in host mode the install touches
nothing.

`gocontroll-usb-hostmode.service` makes the same check before `go-multibus` on
every boot. That is what covers a package installed into a chroot or an image
build, where there is no boot environment to write to: the port is switched on
the first real boot and a module appears on the second. Doing it at install time
is what keeps an `apt-get install` on a running controller down to one reboot.

### From a checkout

```sh
cargo build --release && sudo ./install.sh          # on the controller
```

or cross-build and say where the binaries are:

```sh
cargo zigbuild --release --target aarch64-unknown-linux-musl
BINDIR=target/aarch64-unknown-linux-musl/release ./install.sh
```

`install.sh` puts the same files in the same places as the package, because the
package is built by running it into a staging prefix.

### Building the package

```sh
cargo zigbuild --release --target aarch64-unknown-linux-musl
BINDIR=target/aarch64-unknown-linux-musl/release debian/build-deb.sh 0.3.0 dist
```

The same script the release workflow runs, so what CI publishes is what you can
build and inspect locally. Tagging `v*` builds it, attaches it to a GitHub
release, and tells the GOcontroll apt index to pick it up.

### Tests

```sh
cargo test
```

Everything covered is pure logic - the frame codec, the module bus message
format, the configuration file in both forms, the name map, the supply picker -
so the suite runs on any machine and needs neither a controller nor a module.

## USB host mode

The i.MX8 OTG port comes up as a USB *device* by default. A module plugged into
it never enumerates until the port is a host, and `dr_mode = "otg"` with a
runtime role switch is not enough - the chipidea driver switches the controller
but does not fully initialise the PHY, so the high-speed chirp fails and
descriptor transfers die with `-71`. Only `dr_mode = "host"` gives a full host
init.

`gocontroll-usb-hostmode` sets that, in both places a Moduline can boot from:
the `otg_mode` u-boot variable and the Falcon boot args blob. It backs up
everything it touches and verifies what it wrote. Run it without arguments to
see the current state; `--apply` changes it, `--revert` puts the backups back.

## Module firmware

The module needs firmware with `MbProto` and `ModuleBus` - version 0.1.0 or
later. Older images serve the protocol port as a plain RS485 bridge, and the
tooling says so rather than reporting a vague timeout.

The external supply needs **0.2.0 or later**. On an older image everything else
works and the supply row reads `not supported by this firmware`, which is
reported once rather than on every bring-up. On 0.2.0 the connector gets about
20 % more than the setpoint - 6 V for 5 V, 13.2 V for 11 V - because that image
assumed a smaller opamp gain than the board has; 0.3.0 is calibrated to it.

**Two modules on one controller, the runtime isoSPI role and `IDENTITY` need
0.3.0 or later.** With more than one module configured an older image fails
its bring-up with `firmware older than 0.3.0 cannot be told apart from another
module; update it`, because without `IDENTITY` nothing ties its slot to its USB
device. With one module an older image works as before; a configured role it
cannot take is reported, not enforced.

Firmware lives in the
[Module-Multibus](https://github.com/GOcontroll/Module-Multibus) repository,
which is also where the documents under `docs/` are edited.

## Examples

[examples/node-red-multibus-cells.json](examples/node-red-multibus-cells.json)
reads the published cell voltages every 500 ms and emits them as JSON, with one
signal parsed out as an example. See [examples/README.md](examples/README.md).

## Documentation

- [docs/gocontroll-cellmon.md](docs/gocontroll-cellmon.md) - how the cell data
  reaches Linux and how to read it. This is the source for the knowledge base
  article.
- [docs/go-multibus.md](docs/go-multibus.md) - the service, the UI, and the
  go-can integration.
- [docs/multibus-usb-protocol.md](docs/multibus-usb-protocol.md) - frame format
  and command reference.
- [docs/multibus-interfaces.md](docs/multibus-interfaces.md) - which interface
  combinations exist and why.
- [docs/multibus-external-supply.md](docs/multibus-external-supply.md) - the
  regulator on the connector, its range, and why it is the one setting the
  module remembers.

## Exit codes

| Code | Meaning                                          |
|------|--------------------------------------------------|
| 0    | Success                                          |
| 1    | The module answered with an error, or is unreachable |
| 2    | Usage error                                      |

## License

MIT - see [LICENSE](LICENSE).
