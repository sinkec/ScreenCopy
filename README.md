# ScreenCopy

Clone one monitor to a TV over Miracast (Wi-Fi Display) on Linux Mint (Cinnamon, X11).

**Status: early development.** The program mirrors one monitor to a Miracast TV, with sound. It can
also record a monitor to an MPEG-TS file with a live preview.

## Build and run

Requirements: Rust (stable), GTK 4.14 or newer and libadwaita 1.5 or newer, including their
development packages. On Linux Mint 22 or Ubuntu 24.04:

    sudo apt install build-essential libgtk-4-dev libadwaita-1-dev \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-ugly

Run:

    cargo run

Logging goes to stderr and is controlled by `RUST_LOG`, for example `RUST_LOG=debug cargo run`.
To rule out an encoder problem, force one with `SCREENCOPY_ENCODER=x264enc` or
`SCREENCOPY_ENCODER=nvcudah264enc`.

## Firewall helper

Mirroring needs the TV to reach this computer, which a firewall blocks. ScreenCopy never runs as
root and never disables the firewall. A small helper, started through polkit, allows only TCP 7236
(RTSP) and UDP 67 (DHCP), only on the Wi-Fi Direct interface, and only during a session. The helper
removes the rules when ScreenCopy ends for any reason, including a crash. The rules are not
persistent, so a reboot also clears them.

Install the helper and its policy:

    cargo build --release --bin screencopy-firewall-helper
    sudo install -D -m 755 -o root -g root target/release/screencopy-firewall-helper \
        /usr/libexec/screencopy/screencopy-firewall-helper
    sudo install -m 644 -o root -g root data/io.github.sinek.ScreenCopy.policy \
        /usr/share/polkit-1/actions/io.github.sinek.ScreenCopy.policy

With firewalld, also install its zone:

    sudo install -m 644 -o root -g root data/screencopy.firewalld.xml \
        /usr/lib/firewalld/zones/screencopy.xml
    sudo firewall-cmd --reload

## Checks

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

Dependency checks use two extra tools:

    cargo install --locked cargo-audit cargo-deny
    cargo audit
    cargo deny check

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
