#!/bin/sh
# Install go-multibus on a Moduline controller.
#
# Entry points go to /usr/bin without their extension so they read like go-can
# and go-modules; the shared modules go to /usr/lib/gocontroll-multibus, which
# every entry point has on its import path.
#
# Services are installed but not started: the service resets the module and
# brings CAN interfaces up, which is not something an install should do behind
# your back. Run `systemctl enable --now go-multibus` when you want it.
set -e

PREFIX="${PREFIX:-}"
BIN="$PREFIX/usr/bin"
LIB="$PREFIX/usr/lib/gocontroll-multibus"
UNITS="$PREFIX/etc/systemd/system"
RULES="$PREFIX/etc/udev/rules.d"
UDEVLIB="$PREFIX/lib/udev"

here=$(cd "$(dirname "$0")" && pwd)

install -d "$BIN" "$LIB" "$UNITS" "$RULES" "$UDEVLIB"

install -m 755 "$here/bin/go-multibus"          "$BIN/go-multibus"
install -m 755 "$here/bin/gocontroll-cellmon"   "$BIN/gocontroll-cellmon"
install -m 755 "$here/bin/gocontroll-modulebus" "$BIN/gocontroll-modulebus"

install -m 644 "$here/lib/gocontroll_multibus.py"         "$LIB/"
install -m 644 "$here/lib/gocontroll_multibus_service.py" "$LIB/"

install -m 644 "$here/debian/systemd/go-multibus.service"        "$UNITS/"
install -m 644 "$here/debian/systemd/gocontroll-cellmon.service" "$UNITS/"

install -m 644 "$here/debian/udev/81-gocontroll-multibus.rules" "$RULES/"
install -m 755 "$here/debian/udev/gocontroll-multibus-canname"  "$UDEVLIB/"

# Skipped when staging into a PREFIX for a package build: there is no running
# system to reload.
if [ -z "$PREFIX" ]; then
    udevadm control --reload-rules
    udevadm trigger --subsystem-match=net --subsystem-match=tty --action=add
    systemctl daemon-reload
fi

echo "installed. Start the service with: systemctl enable --now go-multibus"
