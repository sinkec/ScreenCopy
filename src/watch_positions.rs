//! Where each film was left, so that watching can go on from there. The file is
//! private and stays on this computer. It holds checksums rather than file names,
//! so that it does not list what was watched.

use std::fs;
use std::path::Path;

use gst::glib;
use tracing::warn;

use crate::settings::{config_file, write_private_file};

const FILE_NAME: &str = "positions";
const MAX_FILE_LENGTH: usize = 64 * 1024;
const MAX_FILMS: usize = 200;
const SEPARATOR: char = '\t';
/// Nothing is lost by starting over after the first moments of a film.
const MIN_POSITION_MS: u64 = 30_000;
/// Close to the end means the credits: the film counts as watched.
const END_MARGIN_MS: u64 = 60_000;

/// The most recently watched film comes first.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WatchPositions {
    films: Vec<(String, u64)>,
}

fn key_for(path: &Path) -> String {
    glib::compute_checksum_for_string(glib::ChecksumType::Sha256, path.to_string_lossy().as_ref())
        .map_or_else(String::new, |sum| sum.to_string())
}

impl WatchPositions {
    /// An unreadable or damaged file simply means that nothing is remembered.
    pub fn load() -> Self {
        config_file(FILE_NAME)
            .and_then(|path| fs::read_to_string(path).ok())
            .filter(|text| text.len() <= MAX_FILE_LENGTH)
            .map_or_else(Self::default, |text| Self::parse(&text))
    }

    fn parse(text: &str) -> Self {
        let films = text
            .lines()
            .filter_map(|line| {
                let (key, position) = line.split_once(SEPARATOR)?;
                Some((key.to_owned(), position.parse().ok()?))
            })
            .take(MAX_FILMS)
            .collect();
        Self { films }
    }

    fn format(&self) -> String {
        self.films
            .iter()
            .map(|(key, position)| format!("{key}{SEPARATOR}{position}\n"))
            .collect()
    }

    fn save(&self) {
        let Some(path) = config_file(FILE_NAME) else {
            warn!("no directory for the watch positions");
            return;
        };
        if let Err(error) = write_private_file(&path, self.format().as_bytes()) {
            warn!(%error, "cannot save the watch positions");
        }
    }

    /// Where this film was left, if that is worth going back to.
    pub fn position_of(&self, path: &Path) -> Option<u64> {
        let key = key_for(path);
        self.films
            .iter()
            .find(|(film, _)| *film == key)
            .map(|(_, position)| *position)
    }

    /// Remembers a position, or forgets the film if it just began or is over.
    pub fn remember(&mut self, path: &Path, position_ms: u64, duration_ms: u64) {
        self.update(path, position_ms, duration_ms);
        self.save();
    }

    fn update(&mut self, path: &Path, position_ms: u64, duration_ms: u64) {
        let key = key_for(path);
        self.films.retain(|(film, _)| *film != key);
        let finished = duration_ms > 0 && position_ms + END_MARGIN_MS >= duration_ms;
        if position_ms >= MIN_POSITION_MS && !finished {
            self.films.insert(0, (key, position_ms));
            self.films.truncate(MAX_FILMS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;

    #[test]
    fn a_position_is_found_again_after_saving() {
        let mut positions = WatchPositions::default();
        positions.update(Path::new("/films/a.mkv"), 600_000, HOUR);
        let loaded = WatchPositions::parse(&positions.format());
        assert_eq!(loaded.position_of(Path::new("/films/a.mkv")), Some(600_000));
        assert_eq!(loaded.position_of(Path::new("/films/b.mkv")), None);
    }

    #[test]
    fn the_file_does_not_list_what_was_watched() {
        let mut positions = WatchPositions::default();
        positions.update(Path::new("/films/secret.mkv"), 600_000, HOUR);
        assert!(!positions.format().contains("secret"));
    }

    #[test]
    fn the_start_and_the_end_of_a_film_are_not_worth_keeping() {
        let path = Path::new("/films/a.mkv");
        let mut positions = WatchPositions::default();
        positions.update(path, MIN_POSITION_MS - 1, HOUR);
        assert_eq!(positions.position_of(path), None);
        positions.update(path, 600_000, HOUR);
        positions.update(path, HOUR - END_MARGIN_MS, HOUR);
        assert_eq!(positions.position_of(path), None);
    }

    #[test]
    fn a_film_of_unknown_length_is_kept() {
        let path = Path::new("/films/a.mkv");
        let mut positions = WatchPositions::default();
        positions.update(path, 600_000, 0);
        assert_eq!(positions.position_of(path), Some(600_000));
    }

    #[test]
    fn damaged_lines_are_skipped_and_the_list_has_a_limit() {
        let parsed = WatchPositions::parse("nonsense\nkey\tabc\ngood\t5\n");
        assert_eq!(parsed.films, [("good".to_owned(), 5)]);
        assert_eq!(
            WatchPositions::parse(&"k\t1\n".repeat(MAX_FILMS * 2))
                .films
                .len(),
            MAX_FILMS
        );
    }
}
