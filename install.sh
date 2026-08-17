#!/bin/sh
# Install go-multibus from a checkout, into the same paths the .deb uses.
#
# For a controller, prefer the package:
#
#     apt-get install go-multibus
#
# This script is for developing against a checkout, and for a board that is not
# on the GOcontroll apt repository.
#
# Services are installed but not started: the service resets the module and
# raises CAN interfaces, which is not something an install should do underneath
# a running machine. Run `systemctl enable --now go-multibus` when you want it.
set -e

PREFIX="${PREFIX:-}"
BIN="$PREFIX/usr/bin"
LIB="$PREFIX/usr/lib/gocontroll-multibus"
UNITS="$PREFIX/usr/lib/systemd/system"
RULES="$PREFIX/usr/lib/udev/rules.d"
UDEVLIB="$PREFIX/usr/lib/udev"
DOC="$PREFIX/usr/share/doc/go-multibus"

here=$(cd "$(dirname "$0")" && pwd)

install -d "$BIN" "$LIB" "$UNITS" "$RULES" "$UDEVLIB" "$DOC"

install -m 755 "$here/bin/go-multibus"             "$BIN/go-multibus"
install -m 755 "$here/bin/gocontroll-cellmon"      "$BIN/gocontroll-cellmon"
install -m 755 "$here/bin/gocontroll-modulebus"    "$BIN/gocontroll-modulebus"
install -m 755 "$here/bin/gocontroll-usb-hostmode" "$BIN/gocontroll-usb-hostmode"

install -m 644 "$here/lib/gocontroll_multibus.py"         "$LIB/"
install -m 644 "$here/lib/gocontroll_multibus_service.py" "$LIB/"

install -m 644 "$here/debian/systemd/go-multibus.service"            "$UNITS/"
install -m 644 "$here/debian/systemd/gocontroll-cellmon.service"     "$UNITS/"
install -m 644 "$here/debian/systemd/gocontroll-usb-hostmode.service" "$UNITS/"

install -m 644 "$here/debian/udev/81-gocontroll-multibus.rules" "$RULES/"
install -m 755 "$here/debian/udev/gocontroll-multibus-canname"  "$UDEVLIB/"

install -m 644 "$here/README.md" "$DOC/"
for doc in "$here"/docs/*.md; do install -m 644 "$doc" "$DOC/"; done
install -m 644 "$here/examples/node-red-multibus-cells.json" "$DOC/"
install -m 644 "$here/examples/README.md" "$DOC/examples.md"

# Skipped when staging into a PREFIX for a package build: there is no running
# system to reload.
if [ -z "$PREFIX" ]; then
    udevadm control --reload-rules
    udevadm trigger --subsystem-match=net --subsystem-match=tty --action=add
    systemctl daemon-reload
fi

echo "installed. Start the service with: systemctl enable --now go-multibus"
