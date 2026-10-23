# The external supply on the Multibus connector

Besides carrying buses, the module can feed whatever is plugged into it from its
own regulator, at a voltage the controller picks between 5 V and 15 V. This is
the analysis behind `MbSupply.c` and `MbStore.c` in the firmware, and behind the
`Supply` row in `go-multibus`.

> The file names in this document - `MbInterfaces.c`, `MbSupply.c`,
> `MbProto.c`, `usbd_composite.c` - are firmware sources and live in the
> [Module-Multibus](https://github.com/GOcontroll/Module-Multibus)
> repository, not here. That repository is also where these documents are
> edited; the copies here are installed by the package.

## The hardware

Two MCU pins drive it, and there is no third:

| Signal    | Pin | Role                                          |
|-----------|-----|-----------------------------------------------|
| `LDO_ADJ` | PA4 | `DAC1_OUT1`. Sets the output voltage.          |
| `LDO_EN`  | PC4 | Regulator enable, active high.                 |

Between the DAC and the regulator sits an opamp stage that amplifies the DAC's
0..3V3 to 0..15 V, so the transfer is linear and the code for a setpoint is a
straight proportion of the 12 bit range:

```
   DAC code = setpoint_mV * 4095 / 15000
```

`MB_SUPPLY_FULL_SCALE_MV` in `MbSupply.h` is the only place that 15000 appears.
A board that measures out differently is trimmed there and nowhere else.

**There is no feedback path from the regulator output back into the MCU.**
Everything the module reports about the supply is the setpoint it was given, not
a measurement of what actually came out. If a load pulls the rail down, nothing
in this chain will notice.

The order in which the two pins are touched is the only subtle part of driving
them. Going up, the DAC is loaded before `LDO_EN` goes high, so the regulator
never starts against whatever the DAC happened to hold. Going down, `LDO_EN`
drops first and the DAC is parked at zero afterwards, so the output is already
released by the time the setpoint moves.

## The range

5 V to 15 V, in steps of a whole volt. Anything in between is refused rather
than silently rounded, so what the controller shows is what the DAC was given.

The three numbers - minimum, maximum and step - travel with every answer to
`SUPPLY_GET` and `SUPPLY_SET`. That is deliberate: `go-multibus` builds its
picker from what the module reports rather than from a second copy of the table,
so a module whose firmware widened the range needs no new build of the
controller tooling, and the picker can never offer a voltage the module would
refuse.

## Why the setting is persistent

Almost nothing else on this module is. The interface configuration is pushed
over the module bus on every bring-up, because the controller has to know which
CAN channels and which serial ports are real before it initialises them anyway -
see [go-multibus.md](go-multibus.md).

The supply is the exception. A sensor hanging off the connector should get its
voltage back when the module is reset, without waiting for Linux to come round
and say so, so `MbSupply_Init()` restores it from flash before anything else in
the application starts. A module that has never been configured comes up with
the regulator off, so an unconfigured board can never put a voltage on the
connector.

### Where it is stored

`MbStore.c` keeps a 16 byte record in the last page of flash - `0x0805F800`
with the flash in dual bank mode (the factory setting), `0x0803F800` in single
bank mode:

| Offset | Size | Field                                                        |
|--------|------|--------------------------------------------------------------|
| 0-1    | 2    | Magic `0x424D`, so a blank page is not mistaken for a record  |
| 2      | 1    | Record version, currently 1                                   |
| 3      | 1    | Supply enabled                                                |
| 4-5    | 2    | Setpoint in mV                                                |
| 6-13   | 8    | Reserved, room for later settings                             |
| 14-15  | 2    | CRC-16/XMODEM over offsets 0..13                              |

Three details decide how that is written:

- **Programming granularity is a double word.** The record is sized to a
  multiple of eight bytes and goes out as two `HAL_FLASH_Program()` calls.
- **The page geometry depends on an option byte.** With `DBANK` set - the
  factory setting on a 256 KB part - the flash is two banks of 128 KB with 2 KB
  pages; with it clear, one bank with 4 KB pages. `store_addr()` and
  `erase_page()` work that out at runtime from `FLASH->OPTR` instead of
  assuming. In single bank mode the linker scripts reserve the top **4 KB**
  (`LENGTH = 236K` in `STM32G474RCTX_FLASH_BOOT.ld`).
- **Bank 2 does not follow bank 1.** On a 256 KB part in dual bank mode, bank 2
  starts at `0x08040000` - the same address it has on a 512 KB part - so its
  last page is `0x0805F800`, not `0x0803F800`. Up to 0.2.0 the record was
  written at `0x0803F800` while the erase went to the last page of bank 2:
  the first save landed on blank flash and worked, every later one hit flash
  that had never been erased and failed, so the module reported `stored: 0`
  and came back from a reset with the first value it had ever been given.
- **A save only happens when something changed.** `MbStore_Save()` compares
  before it erases, so a controller pushing the same configuration on every boot
  costs no erase cycles at all.

Adding a setting later means taking bytes out of the reserved block and bumping
`MB_STORE_VERSION`. An older record still validates, so `MbStore_Load()` hands
it back and the caller fills the new fields with its own defaults.

**A firmware upload leaves it alone.** The bootloader erases 96 KB from
`0x08004000` and the record lies outside that, so a module comes out of a
firmware update with the supply it had before. The controller pushes its own
value on the next bring-up anyway.

## Who wins: the module or the controller

Both hold a copy, and they are for different moments:

| Copy | Where | What it is for |
|---|---|---|
| Module | flash, restored by `MbSupply_Init()` | The supply coming back on its own after a reset, before or without Linux |
| Controller | `supply_enabled` and `supply_mv` in `/etc/gocontroll/multibus.json` | What the machine is supposed to be |

**The controller's copy wins.** `go-multibus` pushes it during `configure`, over
the module bus, in the same step as the interface configuration. The two only
differ when someone edited the file while the module was out of its slot, and
then the file should be the one that takes effect.

## Driving it

### From the UI

`go-multibus` shows the supply as the last row of its main list, and Enter opens
a picker with `off` and every volt the module accepts:

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

Unlike an interface change this needs no reset: the module takes the setpoint
immediately and stores it, so Enter both applies and saves. The row's trailing
note says whether the module agrees with the file, and whether the setting made
it into flash.

### From the command line

```console
$ go-multibus supply 9V          # store it, and apply it if the module is up
$ go-multibus supply off
$ gocontroll-cellmon supply      # what the module says, over USB
$ gocontroll-modulebus 8 supply  # the same, over SPI, before USB exists
```

## The protocol

Both transports carry the same ten byte block, in the same order, so the
controller decodes one layout for either link.

### Over USB

`0x50 SUPPLY_GET` takes no payload. `0x51 SUPPLY_SET` takes three bytes: enable,
then the setpoint in mV as a `uint16`. Both answer with the block below, and -
like `IF_SET` - a rejected value comes back as status `bad parameter` together
with the state that is actually live, so a caller never has to guess.

| Offset | Size | Field                                                   |
|--------|------|---------------------------------------------------------|
| 0      | 1    | Enabled                                                  |
| 1-2    | 2    | Setpoint in mV                                           |
| 3-4    | 2    | Minimum setpoint in mV                                   |
| 5-6    | 2    | Maximum setpoint in mV                                   |
| 7-8    | 2    | Step in mV                                               |
| 9      | 1    | Stored: 1 when flash holds this, 0 when it is live only  |

A `stored` of 0 means the write to flash failed. The setting is live either way,
it just will not survive the next reset - worth showing rather than hiding,
which is why it has a bit of its own.

### Over the module bus

`103 SUPPLY_GET` answers with the block at byte 3. `104 SUPPLY_SET` takes enable
at byte 3 and the setpoint at bytes 4-5, and answers with a verdict at byte 3
and the block at byte 4 - the same shape as `101 IF_SET`.

Firmware older than 0.2.0 has neither. Over USB such a module answers `unknown
command`, which `go-multibus` reports once and then leaves alone; over SPI it
simply does not answer, and the controller falls back to USB, which can tell the
two cases apart.

## Related documents

- [multibus-interfaces.md](multibus-interfaces.md) - the connector and the five
  interfaces on it.
- [multibus-usb-protocol.md](multibus-usb-protocol.md) - the full command
  reference for the USB side.
- [go-multibus.md](go-multibus.md) - the service that pushes this during
  bring-up.
