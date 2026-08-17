#!/bin/bash
# Build the go-multibus .deb.
#
# Used by .github/workflows/build-package.yml and runnable by hand, so what CI
# publishes is what you can build and inspect locally:
#
#     debian/build-deb.sh 0.1.0 /tmp/dist
#
# Pure Python, so the package is Architecture: all. dpkg-scanpackages --arch
# arm64 picks up _all.deb as well, so the GOcontroll index carries it unchanged.
set -euo pipefail

VERSION="${1:?usage: build-deb.sh <version> [outdir]}"
DIST="${2:-dist}"

PACKAGE=go-multibus
ARCH=all

here=$(cd "$(dirname "$0")/.." && pwd)
BUILD=$(mktemp -d)/"${PACKAGE}_${VERSION}_${ARCH}"

mkdir -p "${BUILD}/DEBIAN" "${DIST}"

# Everything below /usr comes from install.sh, so the package and a manual
# install cannot drift apart.
PREFIX="${BUILD}" sh "${here}/install.sh" > /dev/null

cat > "${BUILD}/DEBIAN/control" << CONTROL
Package: ${PACKAGE}
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: GOcontroll B.V. <info@gocontroll.com>
Section: misc
Priority: optional
Depends: python3 (>= 3.9), iproute2, udev
Recommends: go-can, can-utils
Homepage: https://gocontroll.com
Description: GOcontroll Multibus module service and utilities
 Brings a Multibus module up after a controller boot, decides what each
 connector pin pair carries (CAN FD, RS485, RS232 or isoSPI), and publishes
 the battery cell voltages it reads over isoSPI to /dev/shm for the rest of
 the system.
 .
 Ships the go-multibus service and interactive UI, gocontroll-cellmon for
 reading cell data, gocontroll-modulebus for the SPI module bus, and
 gocontroll-usb-hostmode to put the controller's OTG port in host mode -
 without which a module never enumerates.
 .
 Deterministic device names (mb_can1..3, /dev/mb_serial1..3, /dev/mb_protocol)
 are installed as udev rules.
CONTROL

cat > "${BUILD}/DEBIAN/postinst" << 'POSTINST'
#!/bin/bash
set -e
case "$1" in
    configure)
        systemctl daemon-reload || true

        # Name devices that are already present.
        udevadm control --reload-rules || true
        udevadm trigger --subsystem-match=net --subsystem-match=tty \
            --action=add || true

        # Enabled, not started. At install time this may be a chroot with no
        # controller under it, and on real hardware the service resets the
        # module and raises CAN interfaces - not something an install should do
        # underneath a running machine.
        systemctl enable gocontroll-usb-hostmode.service || true
        systemctl enable go-multibus.service || true

        echo "go-multibus installed. Start it with:"
        echo "    systemctl start go-multibus"
        echo "On a controller whose OTG port is still in device mode, host mode"
        echo "is applied on the next boot."
        ;;
esac
POSTINST

cat > "${BUILD}/DEBIAN/prerm" << 'PRERM'
#!/bin/bash
set -e
case "$1" in
    remove|purge)
        systemctl stop go-multibus.service || true
        systemctl disable go-multibus.service || true
        systemctl stop gocontroll-cellmon.service || true
        systemctl disable gocontroll-cellmon.service || true
        systemctl disable gocontroll-usb-hostmode.service || true
        ;;
esac
PRERM

cat > "${BUILD}/DEBIAN/postrm" << 'POSTRM'
#!/bin/bash
set -e
case "$1" in
    purge)
        rm -f /etc/gocontroll/multibus.json
        rm -rf /dev/shm/gocontroll
        ;;
esac
# The OTG port is deliberately left in host mode on removal: reverting it needs
# a reboot to take effect, and a package being removed is no reason to change
# how the board boots. `gocontroll-usb-hostmode --revert` does it when that is
# actually wanted.
systemctl daemon-reload || true
udevadm control --reload-rules || true
POSTRM

chmod 755 "${BUILD}/DEBIAN/postinst" "${BUILD}/DEBIAN/prerm" "${BUILD}/DEBIAN/postrm"

dpkg-deb -Zxz --build --root-owner-group "${BUILD}" \
    "${DIST}/${PACKAGE}_${VERSION}_${ARCH}.deb"

echo
echo "built ${DIST}/${PACKAGE}_${VERSION}_${ARCH}.deb"
