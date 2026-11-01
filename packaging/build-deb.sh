#!/bin/sh
# Builds target/debian/screencopy_<version>_<arch>.deb. Needs no root privileges.
set -eu

cd "$(dirname "$0")/.."
APP_ID=io.github.sinkec.ScreenCopy
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
ARCH=$(dpkg --print-architecture)
ROOT=target/debian/root
OUTPUT="target/debian/screencopy_${VERSION}_${ARCH}.deb"

cargo build --release --locked --bin screencopy --bin screencopy-firewall-helper

rm -rf "$ROOT"
install -D -m 755 target/release/screencopy "$ROOT/usr/bin/screencopy"
install -D -m 755 target/release/screencopy-firewall-helper \
    "$ROOT/usr/libexec/screencopy/screencopy-firewall-helper"
install -D -m 644 "data/$APP_ID.policy" "$ROOT/usr/share/polkit-1/actions/$APP_ID.policy"
install -D -m 644 "data/$APP_ID.desktop" "$ROOT/usr/share/applications/$APP_ID.desktop"
install -D -m 644 data/screencopy.firewalld.xml "$ROOT/usr/lib/firewalld/zones/screencopy.xml"
for icon in $(cd data/icons && find hicolor -type f); do
    install -D -m 644 "data/icons/$icon" "$ROOT/usr/share/icons/$icon"
done
install -D -m 644 README.md "$ROOT/usr/share/doc/screencopy/README.md"
install -D -m 644 packaging/copyright "$ROOT/usr/share/doc/screencopy/copyright"

mkdir -p "$ROOT/DEBIAN"
SIZE=$(du -sk "$ROOT" | cut -f1)
sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" \
    packaging/control.in > "$ROOT/DEBIAN/control"

dpkg-deb --root-owner-group --build "$ROOT" "$OUTPUT"
echo "Built $OUTPUT"
