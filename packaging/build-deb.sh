#!/bin/sh
# Builds target/debian/monitorbeam_<version>_<arch>.deb. Needs no root privileges.
set -eu

cd "$(dirname "$0")/.."
APP_ID=io.github.sinkec.MonitorBeam
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
ARCH=$(dpkg --print-architecture)
ROOT=target/debian/root
OUTPUT="target/debian/monitorbeam_${VERSION}_${ARCH}.deb"

cargo build --release --locked --bin monitorbeam --bin monitorbeam-firewall-helper

rm -rf "$ROOT"
install -D -m 755 target/release/monitorbeam "$ROOT/usr/bin/monitorbeam"
install -D -m 755 target/release/monitorbeam-firewall-helper \
    "$ROOT/usr/libexec/monitorbeam/monitorbeam-firewall-helper"
install -D -m 644 "data/$APP_ID.policy" "$ROOT/usr/share/polkit-1/actions/$APP_ID.policy"
install -D -m 644 "data/$APP_ID.desktop" "$ROOT/usr/share/applications/$APP_ID.desktop"
install -D -m 644 data/monitorbeam.firewalld.xml "$ROOT/usr/lib/firewalld/zones/monitorbeam.xml"
for icon in $(cd data/icons && find hicolor -type f); do
    install -D -m 644 "data/icons/$icon" "$ROOT/usr/share/icons/$icon"
done
install -D -m 644 README.md "$ROOT/usr/share/doc/monitorbeam/README.md"
install -D -m 644 packaging/copyright "$ROOT/usr/share/doc/monitorbeam/copyright"
packaging/third-party-licenses.py "$ROOT/usr/share/doc/monitorbeam/THIRD-PARTY-LICENSES"
chmod 644 "$ROOT/usr/share/doc/monitorbeam/THIRD-PARTY-LICENSES"

mkdir -p "$ROOT/DEBIAN"
SIZE=$(du -sk "$ROOT" | cut -f1)
sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" \
    packaging/control.in > "$ROOT/DEBIAN/control"

dpkg-deb --root-owner-group --build "$ROOT" "$OUTPUT"
echo "Built $OUTPUT"
