# Multibus module protocol over USB

Reference for the request/response protocol between Linux and the Multibus
module. The firmware side is `MbProto.c`; the controller side is `src/proto.rs`
in the [go-multibus](https://github.com/GOcontroll/go-multibus) repository.
Both carry the same command table, so extending the protocol means adding an
entry in two places and nothing else.

> The file names in this document - `MbInterfaces.c`, `MbSupply.c`,
> `MbProto.c`, `usbd_composite.c` - are firmware sources and live in the
> [Module-Multibus](https://github.com/GOcontroll/Module-Multibus)
> repository, not here. That repository is also where these documents are
> edited; the copies here are installed by the package.

## Where it runs

The protocol rides on the fourth CDC-ACM port of the module, the one whose USB
control interface number is 7. That port has no UART behind it: bytes written to
it go to the protocol handler and bytes read from it are its answers.

Why a CDC port rather than a dedicated interface is explained in
[multibus-interfaces.md](multibus-interfaces.md); the short version is that all
sixteen USB endpoint addresses of the STM32G474 are already in use, and the
fourth UART bridge could never carry traffic anyway.

Find the port by USB interface number, never by `ttyACM` number - the kernel
hands those out in whatever order it enumerated:

```bash
gocontroll-cellmon list
```

## Frame format

Everything is little endian.

```
 offset  0        1   2      3     4      5 ...            last 2
        +--------+-------+------+------+-----------------+--------+
        | SOF    | LEN   | SEQ  | CMD  | PAYLOAD         | CRC16  |
        | 0x7E   | 2 B   | 1 B  | 1 B  | LEN-2 bytes     | 2 B    |
        +--------+-------+------+------+-----------------+--------+
                           \_________________________/
                            LEN counts these bytes, and the CRC covers exactly them
```

| Field   | Size | Meaning                                                        |
|---------|------|----------------------------------------------------------------|
| SOF     | 1    | Always `0x7E`.                                                  |
| LEN     | 2    | Bytes from SEQ up to the last payload byte, so `payload + 2`.    |
| SEQ     | 1    | Free running request counter. The answer echoes it.              |
| CMD     | 1    | Command code. An answer echoes it with bit 7 set.                |
| PAYLOAD | 0-200| Command specific.                                                |
| CRC16   | 2    | CRC-16/XMODEM over the LEN bytes starting at SEQ.                |

CRC-16/XMODEM: polynomial `0x1021`, initial value `0x0000`, no reflection, no
final xor. Check value over `"123456789"` is `0x31C3`.

**The first payload byte of every answer is a status code.** Answer data starts
at payload byte 1.

A worked example, reading the cell snapshot with sequence number `0x42`:

```
request : 7E 02 00 42 20 CC 4F
response: 7E 4B 00 42 A0 00 <72 bytes of snapshot> <crc>
                      ^^ ^^ ^^
                      |  |  status = ok
                      |  command 0x20 with bit 7 set
                      sequence echoed
```

### Why SEQ matters

A request that timed out is retried. Without the sequence number, the late
answer to the first attempt would be read as the answer to the second one and
every following exchange would be one answer behind. The client discards any
frame whose SEQ or CMD does not match what it is waiting for.

## Status codes

| Code | Name              | Meaning                                          |
|------|-------------------|--------------------------------------------------|
| 0x00 | ok                | Command executed.                                 |
| 0x01 | unknown command   | The firmware has no handler for this CMD.         |
| 0x02 | bad length        | Payload length is wrong for this command.         |
| 0x03 | bad parameter     | A value is out of range or the combination is not supported. |
| 0x04 | not supported     | Wrong role: a slave cannot drive the isoSPI clock. |
| 0x05 | link down         | No valid isoSPI answer has arrived yet.           |
| 0x06 | busy              | The SPI peripheral was in use.                    |

## Commands

### 0x01 INFO

Request: empty. Answer: 14 bytes.

| Offset | Size | Field                                             |
|--------|------|---------------------------------------------------|
| 0      | 1    | Protocol version, currently 1                      |
| 1      | 1    | isoSPI role in force: 0 master, 1 slave/simulator  |
| 2-5    | 4    | Hardware id, `20-30-4-<variant>` as go-modules uses it |
| 6-8    | 3    | Application version, major/minor/patch. 0.2.0 is the first with `SUPPLY_*`, 0.3.0 the first with a runtime role, `IDENTITY`, `LINK_TEST` and CAN FD |
| 9      | 1    | Number of connector interfaces (5)                 |
| 10     | 1    | CDC port carrying this protocol (3)                |
| 11     | 1    | Maximum cell count (24)                            |
| 12     | 1    | Maximum temperature count (4)                      |
| 13     | 1    | Maximum raw isoSPI transfer in bytes (64)          |

Read the role before anything else: `SIM_SET` only works on a slave, and
`ISOSPI_XFER` and `LINK_TEST` only on a master. Up to 0.2.0 the role was fixed
per build configuration; since 0.3.0 it is chosen at runtime with `ROLE_SET`,
and the build only decides the role a module starts in.

### 0x02 IF_GET / 0x03 IF_SET

`IF_GET` takes no payload. `IF_SET` takes exactly five mode bytes, one per
connector interface.

| Mode | Name   | Allowed on         |
|------|--------|--------------------|
| 0    | off    | interfaces 2, 3, 4, 5 |
| 1    | can    | interfaces 1, 3, 4 |
| 2    | rs485  | interfaces 2, 3, 4 |
| 3    | rs232  | interface 2        |
| 4    | isospi | interface 5        |

Both answer with the five modes that are **active after the call**. A rejected
combination comes back as status `bad parameter` together with the unchanged
configuration, so a caller can always tell what the module is actually doing
rather than what it was asked to do.

Interface 5 set to `off` powers the LTC6820 down. To the module on the other end
that looks exactly like a cut isoSPI cable, which makes it the way to stage a
link fault on purpose.

### 0x04 ROLE_GET / 0x05 ROLE_SET

The isoSPI role. `ROLE_GET` takes no payload, `ROLE_SET` takes one byte: 0 for
master, 1 for slave. Both answer with one byte, the role **in force after the
call**; an unknown role comes back as `bad parameter` with the role unchanged.

Changing the role shuts the LTC6820 down, rebuilds SPI2 and the chip select
pin for the other direction and brings it back up; the link counters and the
cell snapshot start over. Asking for the role already in force does nothing,
so pushing it on every bring-up does not drop a working link.

The role is not stored in the module. Like the interface configuration it is
pushed by the controller on every bring-up, so after a reset a module comes
back in its build default (master on a `V20300402` build) until it is told
otherwise. Two modules of the same hardware revision get the same image from
go-modules, which is why the role cannot be a build option any more.

### 0x06 IDENTITY

Request: empty. Answer: 24 bytes.

| Offset | Size | Field                                                   |
|--------|------|---------------------------------------------------------|
| 0-11   | 12   | USB serial number, ASCII, exactly what the device descriptor reports |
| 12-23  | 12   | Chip unique id (UID), raw                               |

The module bus serves the same thing (message 107). That is how the controller
ties a USB device to a slot when there is more than one module: it asks the
slot over SPI who it is, then looks for the USB device with that serial number.

### 0x10 LINK_STATUS

Request: empty. Answer: 28 bytes, packed little endian
(`<BBHIIIIII` in Python terms).

| Offset | Size | Field                                     |
|--------|------|-------------------------------------------|
| 0      | 1    | Role                                      |
| 1      | 1    | Link up (1) or down (0)                   |
| 2-3    | 2    | Configured poll interval in ms            |
| 4-7    | 4    | isoSPI transactions started               |
| 8-11   | 4    | Frames received with a valid CRC          |
| 12-15  | 4    | Frames received with a bad CRC            |
| 16-19  | 4    | SPI peripheral errors                     |
| 20-23  | 4    | Slave re-alignments                       |
| 24-27  | 4    | Age of the last valid frame in ms         |

This is the first thing to look at when the numbers stop moving. `tx_frames`
rising while `rx_frames` stays put means the master is clocking and nothing is
answering - a wiring or a slave problem. Both rising while `crc_errors` also
rises points at signal integrity.

### 0x11 LINK_DIAG

Request: empty. Answer: 32 bytes about the master watchdog, and on a slave how
long re-arming takes.

| Offset | Size | Field                                                   |
|--------|------|---------------------------------------------------------|
| 0      | 4    | Times the watchdog had to reset a stuck transaction      |
| 4      | 4    | SPI2 SR at the last stall                                |
| 8      | 4    | SPI2 CR1                                                 |
| 12     | 4    | SPI2 CR2                                                 |
| 16     | 4    | HAL error code                                           |
| 20     | 2    | Bytes the transmit DMA still had to move                 |
| 22     | 2    | Bytes the receive DMA still had to move                  |
| 24     | 1    | HAL state                                                |
| 25     | 1    | Reserved                                                 |
| 26     | 2    | Slave: longest re-arm after a frame, in us               |
| 28     | 4    | Module tick of the last stall, in ms                     |

A master transaction takes 1.2 ms. One that is still running after 20 ms never
completes - no callback arrives, the handle stays busy and every following
transaction is refused, so the link stops until the role is switched or the
module is reset. That happened twice in 10000 link tests: every byte had been
sent, HAL waited for two more received bytes, and the receive FIFO held one -
with the FIFO threshold at two bytes, the interrupt that would have finished
the transfer never came. The watchdog keeps this record, resets SPI2 and
releases chip select, which costs one transaction instead of the link.

That was with interrupt driven transfers. Since they moved to DMA the stall has
not been seen again - 0 in 2000 link tests alone and in 7500 under load, where
interrupts had seen 51 - and the watchdog stays as a safety net.

A slave re-arms after every frame: SPI2 is reset and armed with the next
answer, with interrupts off. A master that started clocking in that window
would lose the frame, so the slave keeps the longest re-arm it has done - 22 us
on a G474 at 170 MHz - and the master holds the clock off for 30 us after
pulling chip select low. The slave only starts a re-arm while chip select is
high, so it is always ready by the first clock.

### 0x20 CELLS_READ

Request: empty. Answer: 72 bytes of snapshot behind the status byte.

| Offset | Size | Field                                              |
|--------|------|----------------------------------------------------|
| 0      | 1    | Snapshot version, currently 1                       |
| 1      | 1    | Number of valid cells                               |
| 2      | 1    | Number of valid temperatures                        |
| 3      | 1    | Flags, see below                                    |
| 4-7    | 4    | Sample sequence number                              |
| 8-11   | 4    | Age of the sample in ms                             |
| 12-15  | 4    | Pack voltage in mV                                  |
| 16-63  | 48   | 24 cell voltages, `uint16`, 100 uV per step         |
| 64-71  | 8    | 4 temperatures, `int16`, 0.1 degC per step          |

Flags:

| Bit  | Meaning                                     |
|------|---------------------------------------------|
| 0x01 | Link ok: a valid answer arrived in time      |
| 0x02 | Stale: older than three poll intervals       |
| 0x04 | Simulated: produced by the simulator         |
| 0x08 | At least one cell below 2.5000 V             |
| 0x10 | At least one cell above 4.2500 V             |

Voltages travel in 100 uV steps because that is the native unit of the LTC681x
family. A real battery monitor can be dropped in later without the wire format
changing: 36000 means 3.6000 V.

The array is always 24 entries and 4 entries long regardless of how many are
valid, which keeps the structure a fixed size and lets both sides unpack it in
one step.

### 0x21 CELLS_CONFIG

Request: 3 bytes - cell count (1..24), then poll interval in ms as `uint16`
(100..10000, rounded down to a multiple of 100). Answer: status only.

### 0x30 ISOSPI_XFER

Request: 1 to 64 raw bytes. Answer: the same number of bytes, clocked back
within one chip select.

This bypasses the frame protocol entirely and is how a real LTC681x battery
monitor gets driven: build the command word and PEC in Python, send it, read the
answer. It only works on a master build; a slave answers `not supported`.

### 0x31 LINK_TEST

Request: 1 to 80 bytes. Answer: a result byte, the round trip in microseconds
as a `uint32`, then the bytes that came back.

| Result | Meaning                                                    |
|--------|------------------------------------------------------------|
| 0      | ok: the slave echoed exactly what was sent                 |
| 1      | no answer to this request came back within five polls      |
| 2      | something came back but did not validate (CRC)             |
| 3      | a valid answer, with different contents                    |

The master sends the bytes to the slave as an `ECHO` request over isoSPI, waits
12 ms for the slave to arm its answer, and clocks a cell poll to collect it -
up to five times. The slave serves from its 10 ms tick, so the round trip is
typically 13 to 15 ms. Master only; a slave answers `not supported`, and a
master with interface 5 off answers `link down`.

This is what an end-to-end link test is built on: varying lengths and
incrementing contents, counted on the Linux side.

### 0x40 SIM_SET

Request: 3 bytes - cell index (0 based), then voltage in 100 uV steps as
`uint16`. Answer: status only.

Pins one simulated cell to a fixed value. The cell stops following the generated
waveform until the module is reset, which is what makes it possible to drive a
specific cell into an alarm threshold and watch Linux react. Simulator builds
only.

### 0x50 SUPPLY_GET / 0x51 SUPPLY_SET

The external supply the module can put on the connector.

`SUPPLY_GET` takes no payload. `SUPPLY_SET` takes three bytes: enable, then the
setpoint in mV as a `uint16`. Both answer with the same ten byte block:

| Offset | Size | Field                                                   |
|--------|------|---------------------------------------------------------|
| 0      | 1    | Enabled                                                  |
| 1-2    | 2    | Setpoint in mV                                           |
| 3-4    | 2    | Minimum setpoint in mV, currently 5000                   |
| 5-6    | 2    | Maximum setpoint in mV, currently 15000                  |
| 7-8    | 2    | Step in mV, currently 1000                               |
| 9      | 1    | Stored: 1 when flash holds this, 0 when it is live only  |

Like `IF_SET`, a refused value comes back as status `bad parameter` together
with the state that is actually live, so a caller can always tell what the
module is doing rather than what it was asked to do.

The limits travel with every answer on purpose: the controller builds its
picker from what the module reports rather than from a second copy of the
table, so it can never offer a voltage this module would refuse.

**The module stores this in flash and re-applies it on its next start**, which
no other setting does. A `stored` of 0 means the flash write failed - the
setting is live either way, it just will not survive a reset. The whole
subsystem, including where the record lives and what the controller does with
it, is in [multibus-external-supply.md](multibus-external-supply.md).

Firmware older than 0.2.0 answers `unknown command` to both.

### 0x60 ECHO

Answers with the payload it was sent, up to 199 bytes. Served on both
transports: over isoSPI it is what `LINK_TEST` bounces off the slave, over USB
it checks the protocol port itself.

### 0x70 CAN_STATS

Request: empty. Answer: 32 bytes for each of the three gs_usb channels, in
channel order (`mb_can1`, `mb_can2`, `mb_can3`).

| Offset | Size | Field                                                       |
|--------|------|-------------------------------------------------------------|
| 0      | 4    | Frames received on the bus and queued for the host          |
| 4      | 4    | Frames transmitted on the bus and echoed to the host        |
| 8      | 4    | FDCAN receive FIFO overflows                                |
| 12     | 4    | Received frames dropped because the host queue was full     |
| 16     | 4    | Frames from the host dropped: queue full or channel not running |
| 20     | 4    | Times the channel went bus-off                              |
| 24     | 4    | Automatic restarts after bus-off                            |
| 28     | 1    | State: 0 error active, 1 warning, 2 passive, 3 bus-off, 4 stopped |
| 29     | 1    | Transmit error counter (TEC)                                |
| 30     | 1    | Receive error counter (REC)                                 |
| 31     | 1    | 1 when the channel runs                                     |

The counters live outside the USB class state, so they survive a
re-enumeration and say what happened before it.

### 0x71 SERIAL_STATS

Request: empty. Answer: 20 bytes for each of the three serial bridges, in port
order (connector interfaces 2, 3, 4): bytes received on the connector, bytes
sent on the connector, bytes dropped because the host did not keep up, receive
errors (overrun, framing, noise, parity), and of those the overruns - each a
`uint32`.

## Transport behaviour

**Framing.** The port is a byte stream, so a frame can be split across USB
packets and several frames can arrive in one read. Both sides scan for the start
byte, use LEN to find the end, and validate the CRC. Anything that fails
validation costs one byte of resynchronisation, never the whole buffer.

**Timing.** The firmware handles requests from its 10 ms application tick, so a
round trip is a few tens of milliseconds at worst. The client waits one second
and retries twice.

**No unsolicited traffic.** The module never sends anything on its own. If bytes
arrive that were not asked for, they are the tail of an earlier answer and are
discarded by the sequence number check.

## The module bus carries a subset

The controller reaches the same module over SPI before USB exists, using the
bootloader's 46 byte message format rather than the frames above. Only what the
controller needs during bring-up is served there:

| Id  | Message      | Answer                                             |
|-----|--------------|----------------------------------------------------|
| 9   | `IDENTIFY`   | Hardware and software version, byte 13 set when the application is running |
| 100 | `IF_GET`     | Five mode bytes at byte 3                          |
| 101 | `IF_SET`     | Verdict at byte 3, the five active modes at byte 4 |
| 102 | `STATUS`     | Role, link state, cell count, frame counters       |
| 103 | `SUPPLY_GET` | The ten byte supply block at byte 3                |
| 104 | `SUPPLY_SET` | Verdict at byte 3, the supply block at byte 4      |
| 105 | `ROLE_GET`   | isoSPI role at byte 3 (0.3.0)                      |
| 106 | `ROLE_SET`   | Request: role at byte 3. Verdict at byte 3, role in force at byte 4 (0.3.0) |
| 107 | `IDENTITY`   | USB serial number (12 ASCII) at bytes 3-14, chip UID at 15-26 (0.3.0) |

They start at 100 so they can never collide with a bootloader message. Because
SPI is full duplex the answer to a request arrives during the *next* transfer,
so the controller repeats the same message until the matching reply comes back;
that is safe because every one of them is idempotent.

The module resets SPI1 before arming every transfer. The G4 SPI has a four byte
FIFO in each direction, and a transfer that arrived while the module was not
armed used to leave bytes behind in them: every reply after that carried four
stale bytes in front, every request came in shifted and failed its checksum,
and the module bus stayed deaf until the module was reset. Only a peripheral
reset empties those FIFOs.

## The isoSPI side

The same frames travel over the LTC6820 link, padded to a fixed 96 byte
transaction. That is deliberate: it makes an isoSPI transaction a single self
contained event, which matters because the slave resets its SPI peripheral
between transactions to keep byte alignment.

The link is full duplex with no separate turnaround, so **the answer to a
request arrives during the next transaction**, not the current one. The master
lives with that one transaction lag and the sequence number resolves which
request an answer belongs to. At the default 100 ms poll interval the data Linux
reads is therefore at most about 200 ms old, which the `age_ms` field reports
honestly rather than hiding.
