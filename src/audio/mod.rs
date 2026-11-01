//! Sends the computer's sound to the TV while sharing, and puts everything back
//! afterwards.
//!
//! All sound streams move to a temporary virtual output, whose monitor feeds the
//! video stream. What was changed is written to a state file first, so that the
//! next start can undo it if the program ends unexpectedly.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{debug, info, warn};

const SINK_NAME: &str = "screencopy_tv";
/// The source GStreamer reads the virtual output's sound from.
pub const MONITOR_SOURCE: &str = "screencopy_tv.monitor";
const NULL_SINK_MODULE: &str = "module-null-sink";
const STATE_FILE: &str = "audio-route";
const MAX_STATE_LENGTH: usize = 64 * 1024;
/// Streams that start during a session may have a remembered output of their own.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("cannot run pactl: {0}")]
    Run(#[source] io::Error),
    #[error("pactl {command} failed: {message}")]
    Command { command: String, message: String },
    #[error("cannot find a directory for the state file")]
    NoStateDirectory,
    #[error("cannot write the state file: {0}")]
    State(#[source] io::Error),
}

/// Runs `pactl`. Separate so that tests never touch the real sound system.
pub trait Pactl {
    fn run(&mut self, args: &[&str]) -> Result<String, AudioError>;
}

pub struct SystemPactl;

impl Pactl for SystemPactl {
    fn run(&mut self, args: &[&str]) -> Result<String, AudioError> {
        let output = Command::new("pactl")
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .map_err(AudioError::Run)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(AudioError::Command {
                command: args.first().copied().unwrap_or_default().to_owned(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }
}

/// What must be put back. Sink names and indexes come from the local sound
/// server, and only ever reach `pactl` as separate arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RouteState {
    previous_default: String,
    /// Stream index and the index of the output it was moved away from.
    moved: Vec<(u32, u32)>,
}

impl RouteState {
    fn format(&self) -> String {
        let mut text = format!("{}\n", self.previous_default);
        for (stream, sink) in &self.moved {
            text.push_str(&format!("{stream} {sink}\n"));
        }
        text
    }

    /// Ignores damaged lines: restoring what can be read beats restoring nothing.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let previous_default = lines.next()?.trim();
        if previous_default.is_empty() || previous_default.contains(char::is_whitespace) {
            return None;
        }
        let moved = lines
            .filter_map(|line| {
                let (stream, sink) = line.split_once(' ')?;
                Some((stream.parse().ok()?, sink.parse().ok()?))
            })
            .collect();
        Some(Self {
            previous_default: previous_default.to_owned(),
            moved,
        })
    }
}

/// The state file's directory, private to this user as the XDG specification asks.
fn state_path() -> Result<PathBuf, AudioError> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            Some(Path::new(&home).join(".local/state"))
        })
        .ok_or(AudioError::NoStateDirectory)?;
    Ok(base.join("screencopy").join(STATE_FILE))
}

/// Replaces the file in one step, so that a crash cannot leave half of it.
fn write_state(path: &Path, state: &RouteState) -> Result<(), AudioError> {
    let directory = path.parent().ok_or(AudioError::NoStateDirectory)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(AudioError::State)?;
    let temporary = path.with_extension("new");
    let write = || -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(state.format().as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    };
    write().map_err(AudioError::State)
}

fn read_state(path: &Path) -> Option<RouteState> {
    let text = fs::read_to_string(path).ok()?;
    if text.len() > MAX_STATE_LENGTH {
        warn!("ignoring an oversized audio state file");
        return None;
    }
    RouteState::parse(&text)
}

fn remove_state(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        warn!(%error, "cannot remove the audio state file");
    }
}

/// `(stream index, sink index)` from `pactl list short sink-inputs`.
fn parse_streams(text: &str) -> Vec<(u32, u32)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect()
}

/// The index of `name` in `pactl list short sinks`.
fn find_sink(text: &str, name: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let index = fields.next()?.parse().ok()?;
        (fields.next()? == name).then_some(index)
    })
}

/// The index of our module in `pactl list short modules`. Looked up each time,
/// because a remembered index could belong to something else by now.
fn find_our_module(text: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let index = fields.next()?.parse().ok()?;
        let name = fields.next()?;
        let arguments = fields.next()?;
        let ours = name == NULL_SINK_MODULE
            && arguments
                .split_whitespace()
                .any(|argument| argument == format!("sink_name={SINK_NAME}"));
        ours.then_some(index)
    })
}

/// Puts back everything in `state`, as far as it still exists. Each step is
/// independent: one failure must not leave the others undone.
fn restore(pactl: &mut dyn Pactl, state: &RouteState) {
    let default = pactl.run(&["get-default-sink"]);
    // If the user chose another output meanwhile, that choice stands.
    if default.is_ok_and(|name| name.trim() == SINK_NAME)
        && let Err(error) = pactl.run(&["set-default-sink", &state.previous_default])
    {
        warn!(%error, "cannot restore the default output");
    }
    for (stream, sink) in &state.moved {
        let (stream, sink) = (stream.to_string(), sink.to_string());
        if let Err(error) = pactl.run(&["move-sink-input", &stream, &sink]) {
            // Normal: the stream may have ended during the session.
            debug!(%stream, %error, "cannot move a stream back");
        }
    }
    match pactl.run(&["list", "short", "modules"]) {
        Ok(modules) => {
            if let Some(module) = find_our_module(&modules)
                && let Err(error) = pactl.run(&["unload-module", &module.to_string()])
            {
                warn!(%error, "cannot remove the virtual output");
            }
        }
        Err(error) => warn!(%error, "cannot list the sound modules"),
    }
}

/// Undoes a session that ended without cleaning up. Returns whether there was one.
pub fn restore_stale() -> bool {
    let Ok(path) = state_path() else {
        return false;
    };
    restore_stale_at(&mut SystemPactl, &path)
}

fn restore_stale_at(pactl: &mut dyn Pactl, path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match read_state(path) {
        Some(state) => {
            info!("restoring sound after an interrupted session");
            restore(pactl, &state);
        }
        None => warn!("the audio state file is unreadable, ignoring it"),
    }
    remove_state(path);
    true
}

/// Sound is on its way to the TV for as long as this exists.
pub struct AudioRoute {
    pactl: Box<dyn Pactl>,
    path: PathBuf,
    state: RouteState,
    last_refresh: Instant,
}

impl AudioRoute {
    pub fn activate() -> Result<Self, AudioError> {
        Self::activate_with(Box::new(SystemPactl), state_path()?)
    }

    fn activate_with(mut pactl: Box<dyn Pactl>, path: PathBuf) -> Result<Self, AudioError> {
        restore_stale_at(pactl.as_mut(), &path);
        let previous_default = pactl.run(&["get-default-sink"])?.trim().to_owned();
        let mut route = Self {
            pactl,
            path,
            state: RouteState {
                previous_default,
                moved: Vec::new(),
            },
            last_refresh: Instant::now(),
        };
        // Recorded before anything changes. From here on, dropping `route` undoes
        // whatever has been done so far.
        write_state(&route.path, &route.state)?;
        let sink_name = format!("sink_name={SINK_NAME}");
        route.pactl.run(&[
            "load-module",
            NULL_SINK_MODULE,
            &sink_name,
            "rate=48000",
            "channels=2",
            "sink_properties=device.description=ScreenCopy-TV",
        ])?;
        route.pactl.run(&["set-default-sink", SINK_NAME])?;
        route.move_streams()?;
        info!("sound now goes to the TV");
        Ok(route)
    }

    /// Moves streams that are not on the virtual output yet, such as ones that
    /// started just now. Runs on the session's thread, at most every few seconds.
    pub fn refresh(&mut self) {
        if self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return;
        }
        self.last_refresh = Instant::now();
        if let Err(error) = self.move_streams() {
            debug!(%error, "cannot update the sound streams");
        }
    }

    fn move_streams(&mut self) -> Result<(), AudioError> {
        let sinks = self.pactl.run(&["list", "short", "sinks"])?;
        let Some(our_sink) = find_sink(&sinks, SINK_NAME) else {
            warn!("the virtual output has disappeared");
            return Ok(());
        };
        let streams = self.pactl.run(&["list", "short", "sink-inputs"])?;
        let mut changed = false;
        for (stream, sink) in parse_streams(&streams) {
            if sink == our_sink {
                continue;
            }
            let moved = self.pactl.run(&[
                "move-sink-input",
                &stream.to_string(),
                &our_sink.to_string(),
            ]);
            match moved {
                Ok(_) => {
                    debug!(stream, from = sink, "moved a sound stream to the TV");
                    // The first origin counts, if the stream moved around meanwhile.
                    if !self.state.moved.iter().any(|(known, _)| *known == stream) {
                        self.state.moved.push((stream, sink));
                        changed = true;
                    }
                }
                Err(error) => debug!(stream, %error, "cannot move a stream"),
            }
        }
        if changed {
            write_state(&self.path, &self.state)?;
        }
        Ok(())
    }
}

impl Drop for AudioRoute {
    fn drop(&mut self) {
        restore(self.pactl.as_mut(), &self.state);
        remove_state(&self.path);
        info!("sound restored");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    const HEADSET: &str = "headset";
    const SINKS: &str = "5\theadset\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n\
                         6\tspeakers\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n\
                         9\tscreencopy_tv\tPipeWire\tfloat32le 2ch 48000Hz\tIDLE\n";
    const MODULES: &str = "1\tlibpipewire-module-rt\t{\n\
                           \x20   nice.level = -11\n\
                           }\n\
                           7\tmodule-null-sink\tsink_name=other rate=48000\t\n\
                           8\tmodule-null-sink\tsink_name=screencopy_tv rate=48000 channels=2\t\n";

    /// A pretend sound server that records every command.
    #[derive(Clone, Default)]
    struct FakePactl {
        calls: Rc<RefCell<Vec<String>>>,
        default: Rc<RefCell<String>>,
        streams: Rc<RefCell<String>>,
        fail: Rc<RefCell<Vec<&'static str>>>,
    }

    impl FakePactl {
        fn new(streams: &str) -> Self {
            let fake = Self::default();
            *fake.default.borrow_mut() = HEADSET.to_owned();
            *fake.streams.borrow_mut() = streams.to_owned();
            fake
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn called(&self, command: &str) -> bool {
            self.calls().iter().any(|call| call == command)
        }

        fn position(&self, command: &str) -> usize {
            self.calls()
                .iter()
                .position(|call| call == command)
                .unwrap_or_else(|| panic!("not called: {command}: {:?}", self.calls()))
        }
    }

    impl Pactl for FakePactl {
        fn run(&mut self, args: &[&str]) -> Result<String, AudioError> {
            let call = args.join(" ");
            self.calls.borrow_mut().push(call.clone());
            if self
                .fail
                .borrow()
                .iter()
                .any(|prefix| call.starts_with(prefix))
            {
                return Err(AudioError::Command {
                    command: call,
                    message: "refused".to_owned(),
                });
            }
            Ok(match args {
                ["get-default-sink"] => format!("{}\n", self.default.borrow()),
                ["set-default-sink", name] => {
                    *self.default.borrow_mut() = (*name).to_owned();
                    String::new()
                }
                ["list", "short", "sinks"] => SINKS.to_owned(),
                ["list", "short", "modules"] => MODULES.to_owned(),
                ["list", "short", "sink-inputs"] => self.streams.borrow().clone(),
                _ => String::new(),
            })
        }
    }

    /// A private directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let name = format!("screencopy-audio-{}-{name}", std::process::id());
            let path = std::env::temp_dir().join(name);
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }

        fn state(&self) -> PathBuf {
            self.0.join("nested").join(STATE_FILE)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn activate(fake: &FakePactl, dir: &TempDir) -> Result<AudioRoute, AudioError> {
        AudioRoute::activate_with(Box::new(fake.clone()), dir.state())
    }

    #[test]
    fn activation_routes_everything_to_the_virtual_output() {
        let dir = TempDir::new("activate");
        let fake = FakePactl::new("20\t5\t1\tPipeWire\ts16le\n21\t6\t2\tPipeWire\ts16le\n");
        let route = activate(&fake, &dir).unwrap();

        assert!(
            fake.calls().iter().any(
                |call| call.starts_with("load-module module-null-sink sink_name=screencopy_tv")
            )
        );
        assert_eq!(*fake.default.borrow(), SINK_NAME);
        assert!(fake.called("move-sink-input 20 9"));
        assert!(fake.called("move-sink-input 21 9"));
        assert_eq!(route.state.moved, [(20, 5), (21, 6)]);
    }

    #[test]
    fn the_state_is_written_before_anything_changes() {
        let dir = TempDir::new("before");
        let fake = FakePactl::new("");
        *fake.fail.borrow_mut() = vec!["load-module"];
        assert!(activate(&fake, &dir).is_err());
        // The failed route was dropped, which removes the state again.
        assert!(!dir.state().exists());
        assert!(fake.called("load-module module-null-sink sink_name=screencopy_tv rate=48000 channels=2 sink_properties=device.description=ScreenCopy-TV"));
    }

    #[test]
    fn dropping_puts_each_stream_back_where_it_was() {
        let dir = TempDir::new("drop");
        let fake = FakePactl::new("20\t5\t1\tx\ty\n21\t6\t2\tx\ty\n");
        let route = activate(&fake, &dir).unwrap();
        assert!(dir.state().exists());
        drop(route);

        assert_eq!(*fake.default.borrow(), HEADSET);
        assert!(fake.called("move-sink-input 20 5"));
        assert!(fake.called("move-sink-input 21 6"));
        assert!(fake.called("unload-module 8"));
        assert!(!dir.state().exists());
    }

    #[test]
    fn streams_move_back_before_the_virtual_output_goes_away() {
        let dir = TempDir::new("order");
        let fake = FakePactl::new("20\t5\t1\tx\ty\n");
        drop(activate(&fake, &dir).unwrap());
        assert!(fake.position("move-sink-input 20 5") < fake.position("unload-module 8"));
    }

    #[test]
    fn only_our_own_module_is_removed() {
        assert_eq!(find_our_module(MODULES), Some(8));
        assert_eq!(
            find_our_module("7\tmodule-null-sink\tsink_name=other\t\n"),
            None
        );
        assert_eq!(
            find_our_module("7\tmodule-null-sink\tsink_name=screencopy_tv_2\t\n"),
            None
        );
        assert_eq!(
            find_our_module("7\tmodule-other\tsink_name=screencopy_tv\t\n"),
            None
        );
    }

    #[test]
    fn a_default_chosen_by_the_user_during_the_session_stands() {
        let dir = TempDir::new("user-choice");
        let fake = FakePactl::new("");
        let route = activate(&fake, &dir).unwrap();
        *fake.default.borrow_mut() = "speakers".to_owned();
        drop(route);
        assert_eq!(*fake.default.borrow(), "speakers");
    }

    #[test]
    fn refresh_moves_streams_that_started_later_and_remembers_their_origin() {
        let dir = TempDir::new("refresh");
        let fake = FakePactl::new("");
        let mut route = activate(&fake, &dir).unwrap();
        *fake.streams.borrow_mut() = "30\t5\t1\tx\ty\n31\t9\t1\tx\ty\n".to_owned();

        route.refresh();
        assert!(
            !fake.called("move-sink-input 30 9"),
            "must wait for the interval"
        );

        route.last_refresh = Instant::now() - REFRESH_INTERVAL;
        route.refresh();
        assert!(fake.called("move-sink-input 30 9"));
        assert!(
            !fake.called("move-sink-input 31 9"),
            "already on the output"
        );
        assert_eq!(read_state(&dir.state()).unwrap().moved, [(30, 5)]);

        drop(route);
        assert!(fake.called("move-sink-input 30 5"));
    }

    #[test]
    fn first_origin_of_a_stream_is_kept() {
        let dir = TempDir::new("origin");
        let fake = FakePactl::new("30\t5\t1\tx\ty\n");
        let mut route = activate(&fake, &dir).unwrap();
        // An application moved it elsewhere; it is moved over again.
        *fake.streams.borrow_mut() = "30\t6\t1\tx\ty\n".to_owned();
        route.last_refresh = Instant::now() - REFRESH_INTERVAL;
        route.refresh();
        assert_eq!(route.state.moved, [(30, 5)]);
    }

    #[test]
    fn one_failed_restore_step_does_not_stop_the_others() {
        let dir = TempDir::new("failures");
        let fake = FakePactl::new("20\t5\t1\tx\ty\n21\t6\t1\tx\ty\n");
        let route = activate(&fake, &dir).unwrap();
        *fake.fail.borrow_mut() = vec!["set-default-sink", "move-sink-input 20"];
        drop(route);
        assert!(fake.called("move-sink-input 21 6"));
        assert!(fake.called("unload-module 8"));
        assert!(!dir.state().exists());
    }

    #[test]
    fn a_crash_is_undone_by_the_next_start() {
        let dir = TempDir::new("crash");
        let fake = FakePactl::new("20\t5\t1\tx\ty\n");
        let route = activate(&fake, &dir).unwrap();
        // A crash: nothing runs that would clean up.
        std::mem::forget(route);
        assert!(dir.state().exists());
        assert_eq!(*fake.default.borrow(), SINK_NAME);

        let mut next_start = fake.clone();
        assert!(restore_stale_at(&mut next_start, &dir.state()));
        assert_eq!(*fake.default.borrow(), HEADSET);
        assert!(fake.called("move-sink-input 20 5"));
        assert!(fake.called("unload-module 8"));
        assert!(!dir.state().exists());
    }

    #[test]
    fn a_normal_start_does_nothing() {
        let dir = TempDir::new("normal");
        let mut fake = FakePactl::new("");
        assert!(!restore_stale_at(&mut fake, &dir.state()));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_new_session_first_undoes_a_crashed_one() {
        let dir = TempDir::new("after-crash");
        let fake = FakePactl::new("");
        std::mem::forget(activate(&fake, &dir).unwrap());
        let route = activate(&fake, &dir).unwrap();
        // The previous default is the real one, not the leftover virtual output.
        assert_eq!(route.state.previous_default, HEADSET);
    }

    #[test]
    fn a_damaged_state_file_changes_nothing_and_is_removed() {
        let dir = TempDir::new("damaged");
        fs::create_dir_all(dir.state().parent().unwrap()).unwrap();
        fs::write(dir.state(), "two words\n").unwrap();
        let mut fake = FakePactl::new("");
        assert!(restore_stale_at(&mut fake, &dir.state()));
        assert!(fake.calls().is_empty());
        assert!(!dir.state().exists());
    }

    #[test]
    fn state_file_is_private() {
        let dir = TempDir::new("private");
        write_state(&dir.state(), &RouteState::default()).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.state()), 0o600);
        assert_eq!(mode(dir.state().parent().unwrap()), 0o700);
    }

    #[test]
    fn state_round_trips_and_survives_damaged_lines() {
        let state = RouteState {
            previous_default: "alsa_output.usb-x".to_owned(),
            moved: vec![(1, 2), (3, 4)],
        };
        assert_eq!(RouteState::parse(&state.format()), Some(state));

        let parsed = RouteState::parse("sink\n1 2\nnonsense\n-1 2\n3 4\n").unwrap();
        assert_eq!(parsed.moved, [(1, 2), (3, 4)]);
        assert_eq!(RouteState::parse(""), None);
        assert_eq!(RouteState::parse("\n"), None);
    }

    #[test]
    fn parses_pactl_tables_and_ignores_the_rest() {
        assert_eq!(
            parse_streams("20\t5\t1\tPipeWire\ts16le\nbad line\n\n21\tx\t1\n"),
            [(20, 5)]
        );
        assert_eq!(find_sink(SINKS, "screencopy_tv"), Some(9));
        assert_eq!(find_sink(SINKS, "missing"), None);
    }
}
