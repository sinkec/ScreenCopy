# MonitorBeam

Open-source Rust program (package `monitorbeam`) for Linux Mint (Cinnamon, X11) that clones ONE
selected monitor to a TV over Miracast (Wi-Fi Display), with a clean, simple UI similar to the
experience on Windows or Apple. Intended for wide use, so it must be safe and reliable.

**Language: talk to the user in Slovene. Code and comments are in English.**

The program was called ScreenCopy until 2026-11-03. That name was too close to `scrcpy`, which mirrors Android
phones to a computer and is short for "screen copy". SoloCast (a HyperX microphone) and MonitorCast (i-PRO access
control software) were rejected as taken. The local folder may still be called `ScreenCopy`.

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
   and/or tray icon) with a "Stop" button is visible. The one exception, approved by the user: a single
   small picture of each monitor when the window opens and on Refresh, so that monitors can be
   recognized. These stay in memory, and are never saved, sent or repeated in the background.
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

`monitorbeam-firewall-helper` (std only, no project dependencies) runs as root through `pkexec`. Its
stdin pipe stays open for the session, and it removes the rules on EOF, so a crash of the program is
covered. With ufw it inserts non-persistent `iptables` rules into `INPUT` rather than `ufw` rules,
which would be saved to disk. Its root-owned marker `/run/monitorbeam-firewall` records a session and
replaces a state file in XDG directories: it cannot be forged and disappears at boot, like the
rules. The firewalld path needs the installed `monitorbeam` zone and has not been run on a real
firewalld.

## Miracast design

The UI's Share button does, in order: `firewall::is_active`, then `firewall::authorize` if a firewall runs (the
password prompt, before the TV is kept waiting), find the TV again (NetworkManager forgets peers, and auto-search
can be long past), P2P connect, then `wfd::serve` on its own thread (firewall open if needed, listen on the P2P
address only, one accepted connection, M1-M7, capture only after PLAY). With no known firewall, nothing asks for a
password, and the helper is not needed. If the TV does not connect within 12 seconds then, a firewall this program
cannot see is suspected, and it asks to open the firewall after all. Packet IDs must be 0x1011 for
video and 0x1100 for audio, or the TV stays black.
Packets are paced by timestamp (`sync=true`, 100 ms behind real time), found on the real TV to remove
most stutter: measurements showed capture and encoding were steady, and the TV stuttered on uneven
sending. Other causes were tested and excluded on the TV: home Wi-Fi on the PC or TV, key frame distance.
The UDP send buffer is 2 MB, because a key frame (up to about 330 kB) overflowed the default: the system-wide
`SndbufErrors` in `/proc/net/snmp` fell from about 0.7 per second to 0, and the picture improved. Check that
counter first if stutter returns.
The default stays 30 fps: on the real TV, 60 fps showed the ball's bounce more often but stuttered
more. `MONITORBEAM_FRAME_RATE=60` still requests it; a choice in the window could be added later.

## Audio design

`audio::AudioRoute` lives as long as the stream: null sink `monitorbeam_tv`, default output switched,
all sink inputs moved (and rechecked every 2 s, since apps can have remembered outputs). Each stream's
original sink is recorded in `$XDG_STATE_HOME/monitorbeam/audio-route` before changing anything, and is
restored on drop or on the next start after a crash. Tests use `FakePactl` and must never touch the
user's real sound system. On this machine a leftover `gnome_network_displays` null sink from GNOME
Network Displays was the default output; it is not ours, so it is never removed automatically.

## Status and next steps (2026-11-03)

Author: Matej Jazbinšek, GitHub user `sinkec`, repository `sinkec/MonitorBeam` (public, branch `main`).
The installed `.deb` may be older than the working tree, so compare before relying on what the installed
program can do.

Done on 2026-11-03:
- Monitor picker with real size, position and rotation, and one picture per monitor (rule 5 was amended).
- Resolution and frame rate choices, Appearance and Keep Running in the Tray under Preferences.
- Tray icon (`ksni`, `zbus` back on 5, three advisories ignored in `deny.toml`).
- Fixed window of 970 × 630 without a scrollbar.
- Pictures were invisible because converting to RGBA gave alpha 0: now RGBx, also for the recording preview.

Confirmed by the user on the real TV (Samsung AU7172): discovery, connection, firewall with ufw on, picture
with low delay, sound on the TV, sound returning to the headphones after Stop, sharing with or without home
Wi-Fi on the PC or the TV, paced sending removing most stutter, the larger send buffer improving the picture.
On 2026-11-03 also: the tray, stopping the stream, closing the program, Preferences and themes, the Sound
Delay row, and sound in sync with the picture.

The user confirmed on the real TV on 2026-11-03: the redesigned window, searching at startup, the remembered TV, the
TV's saved formats, 720p and 60 fps, sharing with no firewall, crash recovery, Test Recording from the menu, and Stop
while connecting.

Still unconfirmed, and listed in `README.md`: the firewalld path, and asking to open the firewall after 12 seconds.

Version 0.1.0 is released on GitHub with its `.deb`. For a new release, update `CHANGELOG.md` and the version in
`Cargo.toml`, and view any new screenshot first. Ask the user about: GitHub Actions for the checks, and real fuzz
targets. Translations were declined for now: the program stays in English. Flatpak was left out on purpose: the
sandbox conflicts with pkexec, NetworkManager access and X11 capture.

Known on the user's computer: new GTK programs log `Unable to connect to the accessibility bus` until
the next login (see the project memory). A leftover `gnome_network_displays` null sink from GNOME
Network Displays may exist; it is not ours, so it is never removed automatically.

## Window and tray

The window has a fixed size, 970 × 630, and must not need a scrollbar, also on a 1366 × 768 screen. Anything
that appears later must not change its size: a recording's preview takes the monitors' place, and there are no
banners, which used to cover the content. A non-resizable window follows its content's natural size, so a `ScrolledWindow` with
`External` scrollbars holds the size. It clips anything too tall, so check the content's height after changing the
layout: it is exactly 630 now. Only the TV list scrolls, from the fourth TV on, with a visible scrollbar and the
number of TVs in the section's title.

Layout: one spacing of 18 px around and between the parts (`section.rs` sets the rest), and all cards use
libadwaita's `card` or `boxed-list`, so that shadows match. A session's state and Stop are on the TV's row, while
other TVs are hidden. The window's title also says when the screen is being captured. The selection on a monitor is drawn inside the tile, because an outline got cut off by its neighbours.

Google Cast finds more devices than Miracast because it uses the home network, while Miracast needs a TV that
is listening over Wi-Fi Direct. Supporting Cast would be a separate protocol and would need LAN traffic, which
rule 6 does not allow as written, so it needs the user's decision.

When testing windows, act only on the test process's own window id (see the project memory). Blur any
screenshot that shows monitor pictures, because they contain the user's screens.

Searching starts when the window opens: it connects to nothing and captures nothing. The user chose this, and the
README says so, which rule 6 asks for. `tv_memory.rs` saves the last used TV, shown first in the list even before it
is found, and each TV's formats, learned in M3 because a TV gives them only during a connection. The picture rows offer
only the selected TV's known formats. Test Recording is in the window's menu, since most people do not need it, with
Stop on top of its preview. The window stays in English, by the user's choice.

The monitor picker draws monitors in their real size, position and rotation, each with a picture. By
default, closing the window hides it into the tray (`ksni`), and only the tray's Quit or the window
menu's Quit ends the program. While sharing, the tray icon changes and offers Stop Sharing, which rule 5
requires. Without a tray, or during a test recording, closing ends the program as usual. Resolution and
frame rate are wishes: the TV's list arrives only during negotiation, so the closest format is used and
the user is told if it differs. `zbus` stays on the major version `ksni` uses, to avoid two copies.

## Licensing

`packaging/third-party-licenses.py` adds the license texts of all statically linked crates to the
package, and fails the build if one is missing. Run `cargo deny check` and `cargo audit` after changing
dependencies, and keep `deny.toml` compatible with GPL-3.0-or-later.

## Open questions

- The polkit policy uses `auth_admin_keep`, so for a few minutes after the password, other programs of the same
  user could start the helper without asking. `firewall::authorize` depends on it: it asks early, and the firewall
  opens later with no second prompt. Plain `auth_admin` would need that flow changed. The helper's fixed actions
  limit the risk. An independent review of the helper is still worth doing before promoting the program.
- `deny.toml` ignores three "unmaintained" advisories for zbus 5 crates (not vulnerabilities), needed by
  `ksni`. Remove them when `ksni` moves to zbus 6, then move this project back to zbus 6 as well.
- Real fuzz targets (`cargo-fuzz`) need a nightly toolchain, postponed: the user is not familiar with
  them, and deterministic tests with damaged input cover the parsers for now.
