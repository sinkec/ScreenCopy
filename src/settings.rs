//! Saved user settings, in a private file in the XDG configuration directory.

use std::cell::Cell;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tracing::warn;

const SOUND_DELAY_KEY: &str = "sound_delay_ms";
const VIDEO_HEIGHT_KEY: &str = "video_height";
const FRAME_RATE_KEY: &str = "frame_rate";
const THEME_KEY: &str = "theme";
const CLOSE_TO_TRAY_KEY: &str = "close_to_tray";
const TRAY_NOTICE_KEY: &str = "tray_notice_shown";
/// The choices the window offers.
pub const VIDEO_HEIGHTS: [u32; 2] = [1080, 720];
pub const FRAME_RATES: [u32; 2] = [30, 60];
const MAX_FILE_LENGTH: usize = 4096;
/// A larger difference is not a matter of synchronization, and a TV may not
/// buffer more than this.
pub const MAX_SOUND_DELAY_MS: i32 = 500;
/// Moving the picture later also moves the stream's clock, which follows the
/// video. The muxer sets that clock about 125 ms before the picture, so sound
/// moved further than this would reach the TV after its time and could be lost.
pub const MIN_SOUND_DELAY_MS: i32 = -100;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn from_name(name: &str) -> Self {
        match name {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Positive plays the sound later, negative plays the picture later.
    pub sound_delay_ms: i32,
    /// The picture height to ask the TV for. `None` means the recommended one.
    pub video_height: Option<u32>,
    /// The frame rate to ask the TV for. `None` means the recommended one.
    pub frame_rate: Option<u32>,
    pub theme: Theme,
    /// Whether closing the window leaves the program running in the tray.
    pub close_to_tray: bool,
    pub tray_notice_shown: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sound_delay_ms: 0,
            video_height: None,
            frame_rate: None,
            theme: Theme::default(),
            close_to_tray: true,
            tray_notice_shown: false,
        }
    }
}

impl Settings {
    /// Unreadable or damaged settings simply mean the defaults.
    pub fn load() -> Self {
        config_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .filter(|text| text.len() <= MAX_FILE_LENGTH)
            .map_or_else(Self::default, |text| Self::parse(&text))
    }

    pub fn save(&self) {
        let Some(path) = config_path() else {
            warn!("no directory for the settings");
            return;
        };
        if let Err(error) = write_private_file(&path, self.format().as_bytes()) {
            warn!(%error, "cannot save the settings");
        }
    }

    /// Anything unknown or out of range keeps its default.
    fn parse(text: &str) -> Self {
        let value = |key: &str| {
            text.lines()
                .filter_map(|line| line.split_once('='))
                .find(|(name, _)| name.trim() == key)
                .map(|(_, value)| value.trim())
        };
        let flag =
            |key: &str, default: bool| value(key).and_then(|v| v.parse().ok()).unwrap_or(default);
        let allowed = |key: &str, choices: &[u32]| {
            value(key)
                .and_then(|v| v.parse().ok())
                .filter(|number| choices.contains(number))
        };
        let defaults = Self::default();
        Self {
            sound_delay_ms: value(SOUND_DELAY_KEY)
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(0)
                .clamp(MIN_SOUND_DELAY_MS, MAX_SOUND_DELAY_MS),
            video_height: allowed(VIDEO_HEIGHT_KEY, &VIDEO_HEIGHTS),
            frame_rate: allowed(FRAME_RATE_KEY, &FRAME_RATES),
            theme: value(THEME_KEY).map_or(defaults.theme, Theme::from_name),
            close_to_tray: flag(CLOSE_TO_TRAY_KEY, defaults.close_to_tray),
            tray_notice_shown: flag(TRAY_NOTICE_KEY, defaults.tray_notice_shown),
        }
    }

    fn format(&self) -> String {
        let mut text = format!(
            "{SOUND_DELAY_KEY}={}\n{THEME_KEY}={}\n{CLOSE_TO_TRAY_KEY}={}\n{TRAY_NOTICE_KEY}={}\n",
            self.sound_delay_ms,
            self.theme.name(),
            self.close_to_tray,
            self.tray_notice_shown,
        );
        if let Some(height) = self.video_height {
            text.push_str(&format!("{VIDEO_HEIGHT_KEY}={height}\n"));
        }
        if let Some(rate) = self.frame_rate {
            text.push_str(&format!("{FRAME_RATE_KEY}={rate}\n"));
        }
        text
    }
}

/// The settings shared by the parts of the window. Every change is saved at once.
#[derive(Debug, Clone)]
pub struct SettingsStore(Rc<Cell<Settings>>);

impl SettingsStore {
    pub fn load() -> Self {
        Self(Rc::new(Cell::new(Settings::load())))
    }

    pub fn get(&self) -> Settings {
        self.0.get()
    }

    pub fn update(&self, change: impl FnOnce(&mut Settings)) {
        let mut settings = self.0.get();
        change(&mut settings);
        self.0.set(settings);
        settings.save();
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| Some(Path::new(&std::env::var_os("HOME")?).join(".config")))?;
    Some(base.join("screencopy").join("settings"))
}

/// Replaces the file in one step, so that a crash cannot leave half of it. The
/// directory and the file are readable only by this user.
pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let temporary = path.with_extension("new");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let settings = Settings {
            sound_delay_ms: -80,
            video_height: Some(720),
            frame_rate: Some(60),
            theme: Theme::Dark,
            close_to_tray: false,
            tray_notice_shown: true,
        };
        assert_eq!(Settings::parse(&settings.format()), settings);
        assert_eq!(
            Settings::parse(&Settings::default().format()),
            Settings::default()
        );
    }

    #[test]
    fn damaged_settings_mean_the_defaults() {
        for text in [
            "",
            "nonsense",
            "sound_delay_ms=abc",
            "sound_delay_ms=",
            "other=5",
            "video_height=abc",
            "video_height=4320",
            "frame_rate=999",
            "theme=purple",
            "close_to_tray=maybe",
        ] {
            assert_eq!(Settings::parse(text), Settings::default(), "{text:?}");
        }
    }

    #[test]
    fn delay_is_limited_to_the_allowed_range() {
        assert_eq!(Settings::parse("sound_delay_ms=99999").sound_delay_ms, 500);
        assert_eq!(
            Settings::parse("sound_delay_ms=-99999").sound_delay_ms,
            MIN_SOUND_DELAY_MS
        );
    }

    #[test]
    fn closing_to_the_tray_is_on_by_default() {
        assert!(Settings::default().close_to_tray);
        assert!(Settings::parse("").close_to_tray);
        assert!(!Settings::parse("close_to_tray=false").close_to_tray);
    }

    #[test]
    fn written_file_is_private() {
        let directory =
            std::env::temp_dir().join(format!("screencopy-settings-{}", std::process::id()));
        let path = directory.join("nested").join("settings");
        write_private_file(&path, b"x").unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let (file, parent) = (mode(&path), mode(path.parent().unwrap()));
        let contents = fs::read(&path).unwrap();
        let _ = fs::remove_dir_all(&directory);
        assert_eq!((file, parent), (0o600, 0o700));
        assert_eq!(contents, b"x");
    }
}
