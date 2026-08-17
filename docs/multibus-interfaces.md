# Multibus interfaces: which combinations are possible

This is the analysis behind the interface configuration in `MbInterfaces.c` and
behind the USB layout in `usbd_composite.c`. It answers two questions: what can
the connector actually carry at the same time, and does that fit through USB.

> The file names in this document - `MbInterfaces.c`, `usbd_composite.c`,
> `MbProto.c` - are firmware sources and live in the
> [Module-Multibus](https://github.com/GOcontroll/Module-Multibus)
> repository, not here.

## The connector

Pin numbers are for the 26 position uneven slot.

| Interface | Pins  | Modes           | MCU peripheral per mode                | Transceiver control |
|-----------|-------|-----------------|----------------------------------------|---------------------|
| 1         | 6/5   | CAN FD, fixed   | FDCAN3 (PB3/PB4)                       | STB on PB7          |
| 2         | 12/13 | RS485 or RS232  | USART2 (PA2/PA3) or LPUART1 (PC0/PC1)  | RS485-B DE//RE on PB10/PB11 |
| 3         | 18/19 | RS485 or CAN FD | USART1 (PA9/PA10) or FDCAN2 (PB12/PB6) | RS485-A DE//RE on PC6/PC7, STB on PD2 |
| 4         | 24/25 | RS485 or CAN FD | UART4 (PC10/PC11) or FDCAN1 (PB8/PB9)  | RS485-C DE//RE on PB1/PA1, STB on PC12 |
| 5         | -     | isoSPI, fixed   | SPI2 (PB13/14/15) through the LTC6820  | EN on PC3 (v2) / PC0 (v1) |

### Neither the RS485 nor the CAN labels follow the interface numbers

This is the trap in the whole layout, and it costs an afternoon if you miss it.
The board labels count by connector position; the MCU peripheral numbers do not
run in that order. Both families are permuted, and differently:

| Connector | Pins  | Knowledge base | Peripheral | MCU pins  | Linux name        |
|-----------|-------|----------------|------------|-----------|-------------------|
| 1         | 6/5   | CAN 1          | FDCAN**3** | PB3/PB4   | `mb_can1`         |
| 2         | 12/13 | RS485 1 / RS232 1 | USART**2** | PA2/PA3 | `/dev/mb_serial1` |
| 3         | 18/19 | CAN 2          | FDCAN**2** | PB12/PB6  | `mb_can2`         |
| 3         | 18/19 | RS485 2        | USART**1** | PA9/PA10  | `/dev/mb_serial2` |
| 4         | 24/25 | CAN 3          | FDCAN**1** | PB8/PB9   | `mb_can3`         |
| 4         | 24/25 | RS485 3        | UART**4**  | PC10/PC11 | `/dev/mb_serial3` |

Three numbering schemes meet in that table, and they do not agree:

- **Connector interface 1..5** - one per pin pair. Used by `MbInterfaces.c`, by
  `MB_CMD_IF_SET`, and by the `interfaces` array in the service configuration.
- **Knowledge base CAN 1..3 / RS485 1..3** - one per bus, per type. Used for the
  Linux device names, so that what the controller shows matches the datasheet.
- **STM32 peripheral numbers** - neither of the above.

CAN 2 is connector interface 3. That is the one to keep in mind.

The `RS485_A/B/C` names in `main.h` follow the transceivers on the board, so
**A belongs to interface 3 and B to interface 2** - swapped. The CAN peripherals
are reversed end to end: **FDCAN3 is interface 1 and FDCAN1 is interface 4.**

The `FDCANx_STB` pins pair with their own peripheral, so the standby lines
follow the CAN permutation: interface 1 uses `FDCAN3_STB` (PB7), interface 4
uses `FDCAN1_STB` (PC12).

Any code that maps a connector interface to a peripheral or to a control pin has
to go through the table above. There are four such places:

- `can_standby()` and `apply_if2/3/4()` in `MbInterfaces.c`
- `gs_get_fdcan()` and `gs_set_stb()` in `usbd_gs_usb.c`
- the `rs485_pins` table in `usbd_composite.c`
- `Config.can_channels` in `gocontroll_multibus_service.py`

### CAN channel numbering on the Linux side

`gs_get_fdcan()` maps the gs_usb channel index - which is what Linux names
`can0`, `can1` and `can2` - so that it counts connector interfaces in order:

| Channel | Linux name | Knowledge base | Connector interface | Peripheral |
|---------|------------|----------------|---------------------|------------|
| 0       | `mb_can1`  | CAN 1          | 1 (always CAN)      | FDCAN3     |
| 1       | `mb_can2`  | CAN 2          | 3                   | FDCAN2     |
| 2       | `mb_can3`  | CAN 3          | 4                   | FDCAN1     |

The kernel would otherwise name these in discovery order, which differs per
controller - `can4`..`can6` on a Moduline IV with four onboard mcp251x
controllers, `can2`..`can4` on an M1 with two. `81-gocontroll-multibus.rules`
pins them to the names above; see [go-multibus.md](go-multibus.md#device-names).

### CAN and RS485 do not share MCU pins

The two peripherals behind a configurable interface sit on entirely different
MCU pins - FDCAN2 on PB6/PB12 while RS485-A on interface 3 is on PA9/PA10. What
they share is the pair of *connector* pins, through two transceivers wired in
parallel. Selecting a mode therefore means enabling one transceiver and forcing
the other into standby; no GPIO is ever re-muxed between modes.

## The eight combinations

Interface 1 is always CAN and interface 5 is always isoSPI, so the choice is
three independent binary ones:

| # | IF2   | IF3   | IF4   | CAN channels | Serial ports | isoSPI |
|---|-------|-------|-------|--------------|--------------|--------|
| 1 | RS232 | CAN   | CAN   | 3            | 1            | yes    |
| 2 | RS232 | CAN   | RS485 | 2            | 2            | yes    |
| 3 | RS232 | RS485 | CAN   | 2            | 2            | yes    |
| 4 | RS232 | RS485 | RS485 | 1            | 3            | yes    |
| 5 | RS485 | CAN   | CAN   | 3            | 1            | yes    |
| 6 | RS485 | CAN   | RS485 | 2            | 2            | yes    |
| 7 | RS485 | RS485 | CAN   | 2            | 2            | yes    |
| 8 | RS485 | RS485 | RS485 | 1            | 3            | yes    |

Any interface may also be set to `off`, which parks both of its transceivers.

**At most three serial ports can ever be live at once**, and at most three CAN
channels. That single fact is what makes the rest work.

### One combination is refused on V20300401 hardware

On hardware version 1 the LTC6820 enable line sits on PC0, which is also
LPUART1_RX. RS232 and isoSPI cannot both be alive there, so `MbInterfaces_Set()`
rejects that pair on a v1 build. Hardware version 2 moved the enable to PC3 and
has no such restriction.

## Does it fit through USB

The STM32G474 USB peripheral has eight endpoint registers, EP0 to EP7, each
usable as one IN and one OUT endpoint. Sixteen endpoint addresses, no more -
`dev_endpoints = 8` in `usbd_conf.c` and `PCD_ENDP7` is the highest the HAL
defines.

What the module needs:

| Consumer                         | IN | OUT | Note                                        |
|----------------------------------|----|-----|---------------------------------------------|
| Control                          | 1  | 1   | EP0                                         |
| gs_usb, all CAN channels         | 1  | 1   | EP1; the channel number is inside the frame |
| CDC data, three serial bridges   | 3  | 3   | EP2, EP3, EP4                               |
| CDC data, module protocol        | 1  | 1   | EP5                                         |
| CDC notification, four ports     | 2  | 2   | EP6 and EP7, IN and OUT                     |
| **Total**                        | **8** | **8** | exactly sixteen                          |

It fits exactly, with nothing to spare. Two things make that possible:

- **gs_usb multiplexes.** All three CAN channels share one endpoint pair,
  because the gs_usb frame carries the channel index. Three CAN channels cost
  the same as one.
- **Three serial bridges, not four.** The module has four UART peripherals but
  the connector can only expose three serial interfaces at a time, so a fourth
  CDC-ACM bridge would never carry traffic. That freed endpoint pair is what the
  isoSPI channel runs on.

The notification endpoints deserve a note, because they look wrong at first
glance. Linux `cdc_acm` refuses a control interface with zero endpoints, but the
firmware never sends notifications. Two ports declare a real interrupt IN
endpoint (`0x86`, `0x87`) that simply always NAKs; the other two declare
`0x06` and `0x07`, which are OUT addresses. `cdc_acm` builds an IN pipe from
whatever number it finds there, so those two land on endpoints 6 and 7 IN as
well - the same two NAKing endpoints. Four unique addresses in the descriptor,
two actual endpoints. Nothing else would have fit.

### Why the protocol rides a CDC port and not something else

A fifth CDC-ACM port for isoSPI - the shape originally planned - needs a bulk
IN, a bulk OUT and a notification address: three more than exist. The way out
was not to add a port but to notice that the fourth UART bridge was never
usable, and to give its port to the protocol instead.

That keeps the configuration descriptor byte for byte what it already was. A
CDC-ACM port is a CDC-ACM port whether a UART or a protocol handler sits behind
it, so Linux still sees exactly four `/dev/ttyACM*` nodes and one gs_usb device,
and none of the hard won enumeration behaviour changed.

## The resulting USB layout

| Endpoint | IN                   | OUT                  | Linux                    |
|----------|----------------------|----------------------|--------------------------|
| EP0      | control              | control              | -                        |
| EP1      | gs_usb               | gs_usb               | `can0`, `can1`, `can2`   |
| EP2      | CDC port 0 data      | CDC port 0 data      | first `ttyACM`, interface 2 |
| EP3      | CDC port 1 data      | CDC port 1 data      | second `ttyACM`, interface 3 |
| EP4      | CDC port 2 data      | CDC port 2 data      | third `ttyACM`, interface 4 |
| EP5      | CDC port 3 data      | CDC port 3 data      | fourth `ttyACM`, module protocol |
| EP6      | notification, port 0 | notification, port 2 | -                        |
| EP7      | notification, port 1 | notification, port 3 | -                        |

The layout is fixed and does not depend on the interface configuration. That is
deliberate: a descriptor that changed with the configuration would force a USB
re-enumeration every time the controller reconfigures the module, and the device
node names would move underneath anything already using them.

The consequence is that Linux always sees three CAN interfaces and four serial
ports, whether or not each one has a live transceiver behind it. An interface
configured as RS485 still has its `canX` netdev; bringing it up succeeds but no
frame ever leaves, because `gs_channel_start()` refuses to take that transceiver
out of standby. The same holds the other way around: a serial port whose
interface is configured as CAN accepts writes and drops them.

Do not rely on the `ttyACM` numbers. Find the port by USB interface number
instead, which is what `enumerate_ports()` in `gocontroll_multibus.py` does:
interface 1, 3 and 5 are the serial bridges for connector interfaces 2, 3 and 4,
and interface 7 is the module protocol.

## How a configuration is applied

`MbInterfaces_Set()` validates the combination, switches the transceivers, and
starts or stops the UART behind each CDC port. Interfaces that lose their serial
mode get their UART de-initialised so its TX pin goes high impedance rather than
idling into a transceiver that should be quiet.

Two routes reach it, and both end in the same function:

1. **Over the module bus (SPI), during bring-up.** `MODBUS_MSG_IF_SET` carries
   five mode bytes. This is the route the service uses, because the controller
   has to know which interfaces are real *before* it initialises them, and USB
   is not up at that point. See `ModuleBus.c` and
   [go-multibus.md](go-multibus.md).
2. **Over USB, at any time afterwards.** `MB_CMD_IF_SET` carries the same five
   bytes. Convenient for experimenting, and it is what
   `gocontroll-cellmon interfaces --set can,rs485,can,rs485,isospi` uses.

The power-on configuration comes from `MB_IF2_DEFAULT`, `MB_IF3_DEFAULT` and
`MB_IF4_DEFAULT` in `MbInterfaces.c`, currently all RS485. Nothing is stored in
flash, so the module always starts from that default and the controller reapplies
whatever it wants after every module reset. That is deliberate: the controller
owns the configuration, so there is only ever one authoritative copy of it and no
way for a module to come back from the field configured differently than the
machine it is in expects.

## Interface 2 is not switchable in firmware

The RS232 driver has only TX and RX brought out to the MCU; the rest of its pins
are strapped to GND or 3V3 on the board. It therefore has **no shutdown input**
and drives its output whenever the board is powered.

That breaks the pattern the other interfaces follow. Everywhere else, selecting a
mode means putting the transceiver that must stay quiet into standby:

| Interface | Quieting the unselected driver                          |
|-----------|---------------------------------------------------------|
| 3, 4      | CAN transceiver STB high, or RS485 DE low and /RE high   |
| 2         | **not possible** - the RS232 driver has no such pin      |

All `apply_if2()` can do is de-initialise LPUART1 so PC1 goes high impedance.
A floating input does not silence an RS232 driver; it settles at its idle level
and keeps driving. So in RS485 mode the RS232 driver still sits on connector pins
12/13 as a low-impedance ±5 V source while RS485-B tries to drive the same pair.
Its receiver adds a load too - RS232 receivers are specified at 3 to 7 kΩ to
ground, heavier than the 12 kΩ RS485 unit load - but the driver is the real
problem.

**The consequence: interface 2's mode is a board-level choice, not a runtime
one.** Selecting RS485 there is only safe on a board where the RS232 driver is
not fitted, and vice versa.

Three ways forward, in the order they are worth considering:

1. **Add a shutdown line in hardware.** Most RS232 transceivers have a `SHDN` or
   `EN` pin; routing it to a spare GPIO instead of strapping it makes interface 2
   behave like the others, and `apply_if2()` becomes three lines. This is the
   only option that makes the interface genuinely configurable.
2. **Fit one driver per board variant** and build the firmware with
   `-DMB_IF2_LOCKED` plus the matching `-DMB_IF2_DEFAULT`. Validation then
   refuses any request to change interface 2, so a controller cannot put a board
   into a combination its hardware does not support. A request that asks for the
   mode already in place still succeeds, so pushing a full five-byte
   configuration works unchanged.
3. **Leave it as is** and accept that interface 2 must not be reconfigured in the
   field. This is the current default, and it is only safe because the boards in
   use today have one driver populated.

Note that the firmware default (`MB_IF2_DEFAULT`) is RS485. On a board with both
drivers fitted, that default is already the conflicting combination - the module
does not have to be reconfigured for it to happen.

## Confirmed against the hardware

- `FDCAN1_STB` is on PC12, alongside FDCAN1 on PB8/PB9. The standby lines pair
  with their own peripheral, not with the connector position, which is what
  `gs_set_stb()` and `can_standby()` assume.
- The RS232 driver has no shutdown input, as described above.
