# Multibus module protocol over USB

Reference for the request/response protocol between Linux and the Multibus
module. The firmware side is `MbProto.c`, the Python side is
`lib/gocontroll_multibus.py`. Both carry the same command table, so
extending the protocol means adding an entry in two places and nothing else.

> The file names in this document - `MbInterfaces.c`, `usbd_composite.c`,
> `MbProto.c` - are firmware sources and live in the
> [Module-Multibus](https://github.com/GOcontroll/Module-Multibus)
> repository, not here.

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
| 1      | 1    | Role: 0 master, 1 slave/simulator                  |
| 2-5    | 4    | Hardware id, `20-30-4-<variant>` as go-modules uses it |
| 6-8    | 3    | Application version, major/minor/patch             |
| 9      | 1    | Number of connector interfaces (5)                 |
| 10     | 1    | CDC port carrying this protocol (3)                |
| 11     | 1    | Maximum cell count (24)                            |
| 12     | 1    | Maximum temperature count (4)                      |
| 13     | 1    | Maximum raw isoSPI transfer in bytes (64)          |

Read the role before anything else: `SIM_SET` only exists on a simulator build
and `ISOSPI_XFER` only on a master.

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

### 0x40 SIM_SET

Request: 3 bytes - cell index (0 based), then voltage in 100 uV steps as
`uint16`. Answer: status only.

Pins one simulated cell to a fixed value. The cell stops following the generated
waveform until the module is reset, which is what makes it possible to drive a
specific cell into an alarm threshold and watch Linux react. Simulator builds
only.

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
