# gocontroll-cellmon: reading battery cell voltages from Linux

How the Multibus module makes battery cell voltages available on a GOcontroll
controller, and how to use the Python tooling that reads them.

This document is written to be the source for a knowledge base article. It
describes what the software does and why, not only which commands to type.

## What the chain looks like

```
   battery pack                Multibus module                 controller
  ┌──────────────┐            ┌────────────────┐            ┌──────────────┐
  │ cell monitor │  isoSPI    │ LTC6820        │    USB     │ Linux        │
  │ (LTC681x or  │◄──────────►│ SPI2 + STM32   │◄──────────►│ cdc_acm      │
  │  simulator)  │  1 twisted │ CellMonitor.c  │  CDC-ACM   │ ttyACM       │
  └──────────────┘  pair      └────────────────┘  port 3    └──────────────┘
                                                                   │
                                                          gocontroll-cellmon
                                                                   │
                                                    /dev/shm/gocontroll/…json
```

The module is the isoSPI **master**: it asks, the battery side answers. Nothing
on the battery side pushes data on its own, which is what keeps the timing
predictable and the failure modes obvious - if the master stops asking, the age
of the data starts climbing and everything downstream can see it.

Three separate rhythms are involved, and it helps to keep them apart:

| Loop                    | Interval        | Set by                          |
|-------------------------|-----------------|---------------------------------|
| Module polls the battery over isoSPI | 100 ms, configurable | `CELLS_CONFIG` |
| Python polls the module over USB     | 500 ms, configurable | `--interval`   |
| Application reads the JSON file      | whenever it likes    | the reader     |

They are deliberately decoupled. The module keeps its own most recent snapshot,
so a Python poll never has to wait for a battery round trip - it always gets an
answer immediately, together with an `age_ms` field saying how old that answer
is. A reader that cares about freshness checks `age_ms`; a reader that does not
just reads the numbers.

## The two files

`lib/gocontroll_multibus.py` is the library. It knows the frame format,
the command table and the structure layouts, and it exposes them as ordinary
Python objects. Import this when writing your own software against the module.

`bin/gocontroll-cellmon` is the command line tool and the service.
Use this to bring the chain up, to look at what is going on, and to publish the
measurements for other software.

Neither needs anything outside the Python standard library. The CDC-ACM port is
opened with `os.open` and put into raw mode with `termios` directly rather than
through pyserial, for the same reason `gocontroll-modulebus` talks to spidev
through raw ioctls: a controller image should not need extra packages installed
before its own hardware works.

## Command line use

### Finding the module

```console
# gocontroll-cellmon list
/dev/ttyACM0   usb interface 1  serial bridge for connector interface 2
/dev/ttyACM1   usb interface 3  serial bridge for connector interface 3
/dev/ttyACM2   usb interface 5  serial bridge for connector interface 4
/dev/ttyACM3   usb interface 7  module protocol (isoSPI, configuration)
```

Every other command finds the protocol port by itself, so `--device` is only
needed when more than one Multibus module is plugged in.

**The `ttyACM` numbers are not stable.** They depend on the order in which the
kernel enumerated USB, so a second module or a re-plug can renumber them. The
tooling therefore looks up the port by its USB interface number - interface 7 is
always the protocol port - and anything else should do the same.

### Checking the module answers

```console
# gocontroll-cellmon info
protocol version : 1
role             : master
hardware         : 20-30-4-2
software         : 0.1.0
interfaces       : 5
protocol port    : CDC port 3
max cells        : 24
max temperatures : 4
max raw transfer : 64 bytes
```

If this works, USB is fine and the module is running its application. If it
times out, the problem is on the USB side and the isoSPI link is not the place
to look yet.

### Checking the isoSPI link

```console
# gocontroll-cellmon status
link             : up
poll interval    : 100 ms
last valid frame : 45 ms ago
transactions     : 1832 sent, 1831 received
crc errors       : 0
spi errors       : 0
resyncs          : 0
```

This is the diagnostic that matters when the numbers look wrong. Read it as
follows:

- **`sent` climbing, `received` flat** - the master is clocking but nothing
  answers. Wiring, the transformer, or a slave that is not running.
- **Both climbing, `crc errors` climbing too** - the link works electrically but
  data is being corrupted. Check the bias divider and the pull-ups on MOSI and
  MISO; both were wrong on the first boards.
- **`resyncs` climbing** - the slave keeps losing byte alignment. Some of this is
  normal around the chip select edges; a rate close to the frame rate is not.
- **`link: down` with a large `last valid frame`** - the link worked and stopped.

### Reading the cells

```console
# gocontroll-cellmon cells
sequence 1832, 45 ms old, link ok, simulated
  cell  1  3.6000 V  <- lowest
  cell  2  3.6040 V
  cell  3  3.6080 V
  ...
  cell 12  3.6440 V  <- highest
pack 43.464 V, spread 0.0440 V
temperatures: 25.0 C, 25.3 C, 24.7 C, 25.1 C
```

`watch` keeps doing this on one line per sample, which is the quickest way to
see whether the numbers are live or frozen.

Note the `simulated` flag. It is set by the module, not by the tooling, and it
travels all the way into the published JSON. Simulated data can never be
mistaken for a real measurement by anything downstream, which is the entire
point of carrying the flag rather than relying on people remembering which
firmware is loaded.

### Configuring the interfaces

```console
# gocontroll-cellmon interfaces
interface 1  pins 6/5     can     (can be can)
interface 2  pins 13/14   rs485   (can be rs485/rs232/off)
interface 3  pins 19/18   rs485   (can be rs485/can/off)
interface 4  pins 25/24   rs485   (can be rs485/can/off)
interface 5  pins isoSPI  isospi  (can be isospi/off)

# gocontroll-cellmon interfaces --set can,rs232,can,rs485,isospi
```

The module answers with the configuration that is active afterwards. If it
refuses a combination, the tool says so and shows what the module kept, so a
rejected change never looks like a successful one. Which combinations exist is
in [multibus-interfaces.md](multibus-interfaces.md).

### Driving a real battery monitor

```console
# gocontroll-cellmon raw 03 60 f4 6c
sent    : 03 60 f4 6c
received: 00 00 ff ff
```

`raw` clocks bytes straight through the LTC6820 within one chip select,
bypassing the frame protocol. This is the hook for talking to a real LTC681x:
build the command word and its PEC in Python, send it, read the answer. It makes
it possible to bring a battery monitor up from the controller before any of it
is written in firmware.

## Running as a service

```console
# gocontroll-cellmon serve
```

`serve` polls the module and publishes one JSON document to
`/dev/shm/gocontroll/multibus-cells.json`. Install the unit from
`debian/systemd/gocontroll-cellmon.service` to have it start at boot.

### The published document

```json
{
  "schema": 1,
  "timestamp": 1755340800.123,
  "valid": true,
  "link_ok": true,
  "stale": false,
  "simulated": true,
  "sequence": 1832,
  "age_ms": 45,
  "cell_count": 12,
  "cells_v": [3.6, 3.604, 3.608],
  "cells_100uv": [36000, 36040, 36080],
  "pack_v": 43.464,
  "min_v": 3.6,
  "max_v": 3.644,
  "spread_v": 0.044,
  "temperatures_c": [25.0, 25.3],
  "alarms": {"undervoltage": false, "overvoltage": false},
  "link": {"poll_ms": 100, "tx_frames": 1832, "rx_frames": 1831,
           "crc_errors": 0, "spi_errors": 0, "resyncs": 0,
           "last_rx_age_ms": 45},
  "errors": {"read_failures": 0, "last_error": ""}
}
```

Three things about this document are worth explaining, because they are design
decisions rather than accidents:

**`valid` is the field to branch on.** It is `link_ok && !stale`, so a reader
that only understands one field still cannot act on data that is not there. The
individual flags are kept alongside it for anything that wants to distinguish
"never worked" from "worked and stopped".

**Voltages appear twice.** `cells_v` is rounded and meant for display;
`cells_100uv` is the raw device value with no rounding step in between, for
anything that wants to do arithmetic or store it. Neither is derived from the
other by the reader, so there is no chance of a rounding disagreement.

**Failures are published too.** When a poll fails, the file is replaced by a
document with `valid: false` and the error text, rather than being left as it
was. A reader that only ever sees a stale file cannot tell a dead service from a
dead battery link; this way it can.

### Atomic replacement

The document is written to a temporary file, flushed, and then moved into place
with `os.replace`. On Linux that is atomic, so a reader either sees the previous
document or the new one in full - never half of either. A reader does not need
locking, retries, or any cooperation with the writer; it can just open the file
and parse it.

### Recovery

Everything that can go wrong at runtime is transient: the module can be reset,
USB can re-enumerate, a poll can time out. None of that should stop the service,
so a failure closes the port, publishes the error, waits two seconds and starts
over with a fresh open. That is what recovers the connection after a module
reset without anyone having to restart the service.

## Reading the data from your own software

From Python, the file is the simplest route:

```python
import json

with open("/dev/shm/gocontroll/multibus-cells.json") as handle:
    data = json.load(handle)

if data["valid"]:
    print("lowest cell: %.4f V" % data["min_v"])
```

To talk to the module directly instead, use the library:

```python
from gocontroll_multibus import MultibusLink

with MultibusLink.open() as link:
    snapshot = link.read_cells()
    if snapshot.link_ok and not snapshot.stale:
        print(snapshot.cells_v)
```

Do not run `serve` and your own reader against the same module at the same time.
Two processes writing to one CDC port interleave their requests and each will
see the other's answers.

## Testing without a battery

The second Multibus module runs a simulator build. It answers the master's
requests with generated cell voltages, so the whole chain - isoSPI, USB, the
Python service and whatever reads the JSON - can be tested end to end before a
real pack exists.

Each simulated cell follows a triangle wave of 150 mV around a resting voltage,
with a phase offset per cell so the values never move in lockstep. That makes it
immediately obvious whether the numbers are live or frozen, which a set of fixed
values would not.

To drive a specific value, for instance to check that an alarm threshold works:

```console
# gocontroll-cellmon sim --cell 3 --mv 2400
cell 3 pinned at 2400.0 mV
```

The pinned cell stops following the waveform until the module is reset. Watching
the master afterwards should show `undervoltage` in the flags, since 2.4 V is
below the 2.5 V threshold.

`sim` is sent to the module you are connected to, so it only works when that
module is the simulator. On a master build it comes back as `not supported`.

## Installing

```bash
./install.sh
systemctl enable --now gocontroll-cellmon
```

`gocontroll-cellmon` is a standalone alternative to the `go-multibus` service:
it reads the cell data but does not bring the module up or configure it. Run one
or the other, never both - they would each open the protocol port and read the
other's answers.

A checkout runs without installing, because the entry points in `bin/` add
`../lib` to their import path.

## Troubleshooting

| Symptom | Where to look |
|---------|---------------|
| `no Multibus protocol port found` | `lsusb` should list `1d50:606f`. If it does not, the module is not enumerating - see the USB host mode notes. If it does, check that `cdc_acm` is loaded. |
| `list` shows fewer than four ports | The module enumerated but the descriptor was rejected. `dmesg` will say which interface. |
| `info` times out | The module enumerated but is not running its application - it is probably still sitting in the bootloader. |
| `status` shows link down | isoSPI. Check the counters as described above. |
| Values are all `3.6000 V` and never change | The simulator is not running, or every cell has been pinned. |
| `simulated: true` in production | A simulator build is loaded on the module that should be the master. |
| JSON file never updates | `systemctl status gocontroll-cellmon`, then run `serve` in the foreground to see the errors. |

## Related documents

- [multibus-interfaces.md](multibus-interfaces.md) - which interface
  combinations are possible and why the protocol shares a CDC port.
- [multibus-usb-protocol.md](multibus-usb-protocol.md) - the frame format and the
  complete command reference.
