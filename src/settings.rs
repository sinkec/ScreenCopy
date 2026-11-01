//! Saved user settings, in a private file in the XDG configuration directory.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tracing::warn;

const SOUND_DELAY_KEY: &str = "sound_delay_ms";
const MAX_FILE_LENGTH: usize = 4096;
/// A larger difference is not a matter of synchronization, and a TV may not
/// buffer more than this.
pub const MAX_SOUND_DELAY_MS: i32 = 500;
/// Moving the picture later also moves the stream's clock, which follows the
/// video. The muxer sets that clock about 125 ms before the picture, so sound
/// moved further than this would reach the TV after its time and could be lost.
pub const MIN_SOUND_DELAY_MS: i32 = -100;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// Positive plays the sound later, negative plays the picture later.
    pub sound_delay_ms: i32,
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

    fn parse(text: &str) -> Self {
        let sound_delay_ms = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .find(|(key, _)| key.trim() == SOUND_DELAY_KEY)
            .and_then(|(_, value)| value.trim().parse::<i32>().ok())
            .unwrap_or(0)
            .clamp(MIN_SOUND_DELAY_MS, MAX_SOUND_DELAY_MS);
        Self { sound_delay_ms }
    }

    fn format(&self) -> String {
        format!("{SOUND_DELAY_KEY}={}\n", self.sound_delay_ms)
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
        };
        assert_eq!(Settings::parse(&settings.format()), settings);
    }

    #[test]
    fn damaged_settings_mean_the_defaults() {
        for text in [
            "",
            "nonsense",
            "sound_delay_ms=abc",
            "sound_delay_ms=",
            "other=5",
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
