# Examples

## node-red-multibus-cells.json

Reads the cell voltages the `go-multibus` service publishes and hands them on as
JSON. Import it through the Node-RED menu: **Import → clipboard**, paste the
file, **Import**.

```
every 500 ms ─► read /dev/shm/… ─► JSON ─► valid? ─┬─► debug: cells (full JSON)
                                                   ├─► parse cell 1 ─► debug: cell 1
                                                   └─► debug: not valid
```

The flow only reads the file. The service owns the module and writes it, so the
two run alongside each other without interfering - unlike a second process
opening the protocol port, which would not work.

**No locking is needed.** The service writes to a temporary file and renames it
into place, which is atomic on Linux: a read gets either the previous document
or the new one in full, never half of either.

**Branch on `valid`.** It is `link_ok && !stale`, so one field says whether the
isoSPI link is delivering fresh data. The flow routes invalid samples to their
own debug node rather than letting stale numbers through, which is what the
`valid?` switch is for. Failures are published too - the file is not simply left
at its last good value - so a reader can tell a dead service from a dead battery
link.

**The `parse cell 1` function** is the example of consuming a single signal. It
reads `cells_100uv[0]` rather than `cells_v[0]`: the first is the raw device
value in the 100 uV steps the LTC681x family uses, the second is rounded for
display. Use the raw one for anything that gets stored or calculated with. The
node also shows the live voltage under itself, so the flow is readable at a
glance.

### Prerequisites

```sh
systemctl enable --now go-multibus
```

Without the service the file does not exist and the file-in node reports an
error every tick. If `valid` stays false while the service runs, the USB side is
fine and the isoSPI link is not - check the counters:

```sh
gocontroll-cellmon status
```

`tx_frames` climbing while `rx_frames` stays put means the master is clocking and
nothing is answering on the other end of the isoSPI pair.

### Message shape

The full document is described in
[../docs/gocontroll-cellmon.md](../docs/gocontroll-cellmon.md#the-published-document).
What the second debug node emits:

```json
{
  "volts": 3.6,
  "raw_100uv": 36000,
  "age_ms": 45,
  "simulated": true
}
```

`simulated` is set by the module, not by this flow: it is true when the data
comes from the demo slave rather than from a real battery. It travels all the
way from the firmware so simulated values can never be mistaken for a
measurement.
