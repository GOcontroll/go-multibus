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
# The three tools are Rust binaries, so something has to have built them first.
# Run it on the controller itself:
#
#     cargo build --release && sudo ./install.sh
#
# or cross-build and point BINDIR at the result:
#
#     cargo zigbuild --release --target aarch64-unknown-linux-musl
#     BINDIR=target/aarch64-unknown-linux-musl/release ./install.sh
#
# Services are installed but not started: the service resets the module and
# raises CAN interfaces, which is not something an install should do underneath
# a running machine. Run `systemctl enable --now go-multibus` when you want it.
set -e

PREFIX="${PREFIX:-}"
BIN="$PREFIX/usr/bin"
UNITS="$PREFIX/usr/lib/systemd/system"
RULES="$PREFIX/usr/lib/udev/rules.d"
UDEVLIB="$PREFIX/usr/lib/udev"
DOC="$PREFIX/usr/share/doc/go-multibus"

here=$(cd "$(dirname "$0")" && pwd)

# Where the built binaries are. Checked in the order they are most likely to be
# the ones meant: an explicit BINDIR, then a cross build for the controller,
# then a native one.
if [ -z "${BINDIR:-}" ]; then
    for candidate in \
        "$here/target/aarch64-unknown-linux-musl/release" \
        "$here/target/aarch64-unknown-linux-gnu/release" \
        "$here/target/release"
    do
        if [ -x "$candidate/go-multibus" ]; then
            BINDIR="$candidate"
            break
        fi
    done
fi

if [ -z "${BINDIR:-}" ] || [ ! -x "$BINDIR/go-multibus" ]; then
    echo "no built binaries found. Build them first:" >&2
    echo "    cargo build --release" >&2
    echo "or point BINDIR at an existing build." >&2
    exit 1
fi

install -d "$BIN" "$UNITS" "$RULES" "$UDEVLIB" "$DOC"

install -m 755 "$BINDIR/go-multibus"           "$BIN/go-multibus"
install -m 755 "$BINDIR/gocontroll-cellmon"    "$BIN/gocontroll-cellmon"
install -m 755 "$BINDIR/gocontroll-modulebus"  "$BIN/gocontroll-modulebus"

# The one tool that is still a script. It rewrites the u-boot environment and
# the Falcon boot blob, runs once at install time, and never at runtime - so it
# has nothing to gain from being a binary.
install -m 755 "$here/bin/gocontroll-usb-hostmode" "$BIN/gocontroll-usb-hostmode"

install -m 644 "$here/debian/systemd/go-multibus.service"             "$UNITS/"
install -m 644 "$here/debian/systemd/gocontroll-cellmon.service"      "$UNITS/"
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

echo "installed from $BINDIR"
echo "start the service with: systemctl enable --now go-multibus"

# Reported, not applied - unlike the package, which switches the port during
# configure. This script installs a checkout and deliberately changes nothing
# about how the board boots. Saying it out loud still matters: the symptom of a
# port in the wrong role is a module that never enumerates, with nothing on the
# outside to suggest why.
dr_mode_node=/proc/device-tree/soc@0/bus@32c00000/usb@32e40000/dr_mode
if [ -z "$PREFIX" ] && [ -r "$dr_mode_node" ]; then
    dr_mode=$(tr -d '\0' < "$dr_mode_node")
    if [ "$dr_mode" != host ]; then
        echo
        echo "NOTE: the controller's OTG port booted in $dr_mode mode, not host."
        echo "No module can enumerate over USB until that changes:"
        echo "    gocontroll-usb-hostmode --apply    # then reboot"
    fi
fi
