# go-multibus

GOcontroll Multibus module service and utilities for Moduline controllers
(L4, M1, HMI1) running on i.MX8MM hardware.

Brings a Multibus module up after a controller boot, configures which physical
interface each connector pin pair carries, and publishes the battery cell
voltages it reads over isoSPI to `/dev/shm` for the rest of the system.

Two faces over one state machine: a background service that runs at boot, and a
terminal UI in the style of `go-can` for commissioning a machine. What the UI
shows is what the service does, because both drive the same code.

Pure standard-library Python 3 - no pyserial, no pyusb, nothing to install on
the controller beyond the interpreter it already has.

## CLI

```sh
go-multibus                              # interactive UI (no arguments)
go-multibus daemon                       # background service, systemd entry
go-multibus status                       # one-shot summary, scriptable
go-multibus config                       # show the stored configuration
go-multibus set 3 can                    # change one interface, store it

gocontroll-cellmon list                  # which Multibus devices are present
gocontroll-cellmon info                  # identity, role, capabilities
gocontroll-cellmon status                # isoSPI link counters
gocontroll-cellmon cells                 # one cell snapshot
gocontroll-cellmon watch                 # keep printing snapshots
gocontroll-cellmon interfaces --set can,rs485,can,rs485,isospi
gocontroll-cellmon raw 03 60 f4 6c       # raw isoSPI transfer
gocontroll-cellmon serve                 # publish to /dev/shm

gocontroll-modulebus probe 8             # module bus handshake, over SPI
gocontroll-modulebus start 8             # bootloader -> application
gocontroll-modulebus interfaces 8        # read/write config over SPI
```

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
| `configure` | Push the five interface modes over the module bus                |
| `enumerate` | Wait for USB, open the protocol port, apply the cell configuration |
| `running`   | Poll the cell snapshot and publish it                            |

Every step is idempotent and the sequence restarts from the top when the module
disappears, so pulling a module and putting it back recovers on its own.

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

## Device names

Names are pinned by `debian/udev/81-gocontroll-multibus.rules`, because kernel
names depend on the controller: a Moduline IV has four onboard mcp251x
controllers so the module lands on `can4`..`can6`, while an M1 has two and the
same module lands on `can2`..`can4`. The numbering follows the knowledge base
(CAN 1..3, RS485 1..3), not the connector interface numbers.

The tooling falls back to sysfs discovery when the rule is not installed, but
anything else on the controller that refers to a name needs it.

## Config file

`/etc/gocontroll/multibus.json`. A missing file is a working configuration.

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

`interfaces` is indexed by **connector interface**, so `interfaces[2]` is
interface 3 and drives `mb_can2`.

## Published documents

`/dev/shm/gocontroll/multibus-cells.json` holds the measurements and
`multibus-state.json` where the bring-up sequence stands. Both are written to a
temporary file and moved into place with `os.replace`, which is atomic on Linux:
a reader sees either the previous document or the new one, never half of either,
and needs no locking.

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
install -m 755 bin/go-multibus            /usr/bin/go-multibus
install -m 755 bin/gocontroll-cellmon     /usr/bin/gocontroll-cellmon
install -m 755 bin/gocontroll-modulebus   /usr/bin/gocontroll-modulebus
install -d /usr/lib/gocontroll-multibus
install -m 644 lib/*.py                   /usr/lib/gocontroll-multibus/
install -m 644 debian/systemd/*.service   /etc/systemd/system/
install -m 644 debian/udev/81-gocontroll-multibus.rules /etc/udev/rules.d/
install -m 755 debian/udev/gocontroll-multibus-canname  /lib/udev/
udevadm control --reload-rules && udevadm trigger --action=add
systemctl daemon-reload
```

`./install.sh` does the same thing.

A checkout runs without installing: the entry points in `bin/` add `../lib` to
their import path.

## Module firmware

The module needs firmware with `MbProto` and `ModuleBus` - version 0.1.0 or
later. Older images serve the protocol port as a plain RS485 bridge, and the
tooling says so rather than reporting a vague timeout. Firmware lives in the
[Module-Multibus](https://github.com/GOcontroll/Module-Multibus) repository.

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

## Exit codes

| Code | Meaning                                          |
|------|--------------------------------------------------|
| 0    | Success                                          |
| 1    | The module answered with an error, or is unreachable |
| 2    | Usage error                                      |

## License

MIT - see [LICENSE](LICENSE).
