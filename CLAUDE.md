# ScreenCopy

Open-source Rust program (package `screencopy`) for Linux Mint (Cinnamon, X11) that clones ONE
selected monitor to a TV over Miracast (Wi-Fi Display), with a clean, simple UI similar to the
experience on Windows or Apple. Intended for wide use, so it must be safe and reliable.

**Language: talk to the user in Slovene. Code and comments are in English.**

## Decisions (do not change without asking)

- UI: GTK4 + libadwaita (`gtk4-rs`, `libadwaita-rs`). Use at most features `v4_14` and `v1_5`.
- Protocol: our own Miracast source in Rust:
  - Wi-Fi Direct (P2P) via NetworkManager's D-Bus API (`zbus`). Chosen over wpa_supplicant because
    it needs no `netdev` group and handles IP addressing,
  - RTSP/WFD negotiation (M1 to M7),
  - H.264 video in MPEG-TS over RTP/UDP via GStreamer (`gstreamer-rs`).
- Clone a single monitor only, no virtual displays.
- X11 capture: GStreamer `ximagesrc` with a region (`startx`/`starty`/`endx`/`endy`) computed from
  monitor data read through `x11rb` RandR.
- Audio (later): switch to the matching output on connect, restore it when the session ends.
- License: GPL-3.0-or-later.

## Security rules (mandatory)

1. The program does NOT run as root. Privileged operations (firewall) go through polkit with fixed,
   narrowly defined actions, or through a small separate helper with minimal privileges.
2. Firewall rules apply only to the P2P interface and only during a session. They must be removed
   after the session ends, when the window closes, on error and on crash. Record the state in a
   file so that the next start restores it automatically if the program ended unexpectedly. Never
   disable ufw as a whole.
3. Run external commands only with `std::process::Command` and separate arguments, never through a
   shell or by building strings from user input.
4. The RTSP/WFD server binds only to the P2P interface, never to 0.0.0.0. Everything received from
   the TV is untrusted: length limits, timeouts, robust parsing, no `panic!` on input. Add tests and,
   if possible, fuzz tests for the parser.
5. Screen capture starts only on an explicit user action. While sharing, a clear indicator (window
   and/or tray icon) with a "Stop" button is visible.
6. No telemetry and no network calls other than the connection to the TV. No hidden actions.
7. No `unsafe` unless it is necessary and justified by a comment.
8. Minimal dependencies, vetted libraries, `Cargo.lock` in git. Provide `cargo clippy -D warnings`,
   `cargo fmt`, `cargo test`, and instructions for `cargo audit` and `cargo deny`.
9. Handle errors with `Result` (`thiserror`/`anyhow`), no `unwrap()` in production code. Log with
   `tracing`, no sensitive data in logs. Level is set through `RUST_LOG`.
10. State and configuration files live in XDG directories with appropriate permissions.

## Workflow

- Work in small steps (milestones below). After each one, `cargo check`, `cargo clippy` and
  `cargo test` must pass; then explain what the user got and how to run it.
- Present a short plan before writing code for a milestone.
- The TV and Wi-Fi Direct cannot be tested from here. The user tests on the computer and pastes
  logs (`RUST_LOG=debug`) and errors, so protocol code needs good logging.
- The user delegated technical choices: decide what is best, state the choice, and ask only for
  information you need. Installing anything system-wide or adding privileges still needs their
  approval.
- Never run `sudo` or change system files without explicit permission. Write out the command and
  explain it instead.
- Do not work outside the project folder. After each milestone, suggest a `git commit` message.

## Milestones

1. Skeleton: `Cargo.toml`, module structure (`src/ui`, `src/capture`, `src/firewall`, `src/p2p`,
   `src/wfd`), GTK4/libadwaita window, monitor list (name, resolution, position), monitor selection.
2. Capture: GStreamer pipeline that captures the selected monitor and writes H.264 to a file (test
   without a TV), preview in the window.
3. Firewall: module via polkit/D-Bus (ufw and firewalld), automatic restore, tests.
4. Wi-Fi Direct: device discovery and connection via NetworkManager D-Bus.
5. Miracast/WFD: RTSP negotiation and streaming to the TV.
6. Audio, tray icon, error handling, nicer UI, packaging (Flatpak/.deb), documentation.

## Environment

- Linux Mint 22 (Ubuntu 24.04), Cinnamon on X11, NVIDIA GPU.
- Rust 1.99.1, GTK 4.14.5, libadwaita 1.5.0, GStreamer 1.24.2. `cargo` is in `~/.cargo/bin`, which
  may not be on `PATH` in a non-interactive shell.
- Three monitors, useful as test data (note the portrait one and the non-zero Y offset):
  `DP-0` 2560x1440+0+142, `DP-2` 1080x1920+2560+0, `HDMI-0` 1920x1080+3640+840.
- Encoders that work: `nvcudah264enc`, `x264enc`. `nvh264enc` is installed but fails with driver
  595.99.02 ("Selected preset not supported"). `gtk4paintablesink` is NOT installed and is not in
  the distribution's repositories.
- Wi-Fi interface `wlo1` supports P2P-client, P2P-GO and P2P-device. NetworkManager 1.46 is active
  and already exposes `p2p-dev-wlo1`.
- TV: Samsung AU7172, 75 inch. Screen Mirroring (Miracast) works from Windows. NetworkManager reports
  it as "Samsung AU7172 75 TV", WFD IEs `00 00 06 01 11 1C 44 00 36` (primary sink, available, RTSP
  7236, 54 Mbps).
- GNOME Network Displays connects only with ufw disabled, and captures all three monitors.
- ufw is active; firewalld is not.

## Firewall design

`screencopy-firewall-helper` (std only, no project dependencies) runs as root through `pkexec`. Its
stdin pipe stays open for the session, and it removes the rules on EOF, so a crash of the program is
covered. With ufw it inserts non-persistent `iptables` rules into `INPUT` rather than `ufw` rules,
which would be saved to disk. Its root-owned marker `/run/screencopy-firewall` records a session and
replaces a state file in XDG directories: it cannot be forged and disappears at boot, like the
rules. The firewalld path needs the installed `screencopy` zone and has not been run on a real
firewalld.

## Miracast design

The UI's Share button does, in order: `firewall::authorize` (the password prompt, before the TV is
kept waiting), P2P connect, then `wfd::serve` on its own thread (firewall open, listen on the P2P
address only, one accepted connection, M1-M7, capture only after PLAY). Packet IDs must be 0x1011 for
video and 0x1100 for audio, or the TV stays black.

## Audio design

`audio::AudioRoute` lives as long as the stream: null sink `screencopy_tv`, default output switched,
all sink inputs moved (and rechecked every 2 s, since apps can have remembered outputs). Each stream's
original sink is recorded in `$XDG_STATE_HOME/screencopy/audio-route` before changing anything, and is
restored on drop or on the next start after a crash. Tests use `FakePactl` and must never touch the
user's real sound system. On this machine a leftover `gnome_network_displays` null sink from GNOME
Network Displays was the default output; it is not ours, so it is never removed automatically.

## Status (2026-11-01)

Confirmed by the user on the real TV: discovery, connection, firewall, picture with low delay, sound.
The Debian package is built by `packaging/build-deb.sh`. Not yet confirmed: sound and picture staying in sync, sound returning after Stop, recovery after a
crash, closing the window or Ctrl+C while sharing, the Record button. Author: Matej Jazbinšek, GitHub user `sinkec`.

## Open questions

- Real fuzz targets (`cargo-fuzz`) need a nightly toolchain, postponed: the user is not familiar with
  them, and deterministic tests with damaged input cover the parsers for now.
