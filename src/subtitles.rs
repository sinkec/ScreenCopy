//! Reads SRT subtitles. The files come from the internet, so nothing about them is
//! trusted: their size and contents are limited, and damaged parts are skipped.

use std::path::Path;

use gst::glib;
use thiserror::Error;

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CUES: usize = 50_000;
const MAX_CUE_CHARACTERS: usize = 512;
/// Cues can overlap, so a lookup also checks a few before the latest one.
const OVERLAP_LOOKBACK: usize = 8;
const TIME_ARROW: &str = "-->";
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
const UTF16_LE_BOM: [u8; 2] = [0xFF, 0xFE];
const UTF16_BE_BOM: [u8; 2] = [0xFE, 0xFF];
/// What Slovene and other Central European subtitles usually use when they are
/// not in Unicode. Plain ASCII text reads the same in it.
const LEGACY_ENCODING: &str = "WINDOWS-1250";

#[derive(Debug, Error)]
pub enum SubtitleError {
    #[error("cannot read the subtitles: {0}")]
    Read(#[from] std::io::Error),
    #[error("the subtitle file is too large")]
    TooLarge,
    #[error("no subtitles found in this file")]
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

/// Cues sorted by the time they start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtitles {
    cues: Vec<Cue>,
}

impl Subtitles {
    pub fn load(path: &Path) -> Result<Self, SubtitleError> {
        if std::fs::metadata(path)?.len() > MAX_FILE_BYTES {
            return Err(SubtitleError::TooLarge);
        }
        Self::parse(&std::fs::read(path)?)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, SubtitleError> {
        let mut cues = parse_cues(&decode(bytes));
        if cues.is_empty() {
            return Err(SubtitleError::Empty);
        }
        cues.sort_by_key(|cue| cue.start_ms);
        Ok(Self { cues })
    }

    /// The text to show at `time_ms`, as plain text.
    pub fn text_at(&self, time_ms: u64) -> Option<&str> {
        let later = self.cues.partition_point(|cue| cue.start_ms <= time_ms);
        self.cues[..later]
            .iter()
            .rev()
            .take(OVERLAP_LOOKBACK)
            .find(|cue| cue.end_ms > time_ms)
            .map(|cue| cue.text.as_str())
    }
}

/// Unicode if the file says so or is valid as such, or else the legacy encoding.
fn decode(bytes: &[u8]) -> String {
    if let Some(text) = bytes.strip_prefix(&UTF8_BOM) {
        return String::from_utf8_lossy(text).into_owned();
    }
    if let Some(text) = bytes.strip_prefix(&UTF16_LE_BOM) {
        return convert(text, "UTF-16LE");
    }
    if let Some(text) = bytes.strip_prefix(&UTF16_BE_BOM) {
        return convert(text, "UTF-16BE");
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => convert(bytes, LEGACY_ENCODING),
    }
}

fn convert(bytes: &[u8], encoding: &str) -> String {
    match glib::convert(bytes, "UTF-8", encoding) {
        Ok((converted, _)) => String::from_utf8_lossy(&converted).into_owned(),
        Err(error) => {
            tracing::debug!(%error, encoding, "cannot convert the subtitles");
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

fn parse_cues(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((start_ms, end_ms)) = parse_times(line) else {
            continue;
        };
        let mut lines_of_cue = Vec::new();
        while let Some(next) = lines.peek() {
            if next.trim().is_empty() || parse_times(next).is_some() {
                break;
            }
            lines_of_cue.push(clean_line(next));
            lines.next();
        }
        let text: String = lines_of_cue
            .join("\n")
            .trim()
            .chars()
            .take(MAX_CUE_CHARACTERS)
            .collect();
        if !text.is_empty() && end_ms > start_ms {
            cues.push(Cue {
                start_ms,
                end_ms,
                text,
            });
            if cues.len() == MAX_CUES {
                break;
            }
        }
    }
    cues
}

/// Reads `00:01:02,500 --> 00:01:04,000`, which can have more text after it.
fn parse_times(line: &str) -> Option<(u64, u64)> {
    let (start, end) = line.split_once(TIME_ARROW)?;
    Some((
        parse_time(start)?,
        parse_time(end.split_whitespace().next()?)?,
    ))
}

fn parse_time(text: &str) -> Option<u64> {
    let (clock, milliseconds) = text.trim().split_once([',', '.'])?;
    let mut parts = clock.split(':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds: u64 = parts.next()?.parse().ok()?;
    let milliseconds: u64 = milliseconds.parse().ok()?;
    if parts.next().is_some() || minutes > 59 || seconds > 59 || milliseconds > 999 {
        return None;
    }
    hours
        .checked_mul(3600)?
        .checked_add(minutes * 60)?
        .checked_add(seconds)?
        .checked_mul(1000)?
        .checked_add(milliseconds)
}

/// Removes tags like `<i>`, `</font>` and `{\\an8}`, keeping only the words. A `<`
/// that starts no tag, as in `5 < 6`, is text.
fn clean_line(line: &str) -> String {
    let mut clean = String::with_capacity(line.len());
    let mut closing = None;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        let next = characters.peek().copied().unwrap_or(' ');
        match (closing, character) {
            (None, '<') if next.is_ascii_alphabetic() || next == '/' => closing = Some('>'),
            (None, '{') if next == '\\' => closing = Some('}'),
            (None, other) => clean.push(other),
            (Some(end), other) if end == other => closing = None,
            (Some(_), _) => {}
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "1\n00:00:01,000 --> 00:00:03,000\nFirst line\nSecond line\n\n\
                          2\n00:00:05,500 --> 00:00:06,000\nNext\n\n";

    fn parse(text: &str) -> Subtitles {
        Subtitles::parse(text.as_bytes()).unwrap()
    }

    #[test]
    fn shows_the_right_text_at_the_right_time() {
        let subtitles = parse(SAMPLE);
        assert_eq!(subtitles.text_at(999), None);
        assert_eq!(subtitles.text_at(1000), Some("First line\nSecond line"));
        assert_eq!(subtitles.text_at(2999), Some("First line\nSecond line"));
        assert_eq!(subtitles.text_at(3000), None);
        assert_eq!(subtitles.text_at(5600), Some("Next"));
        assert_eq!(subtitles.text_at(9000), None);
    }

    #[test]
    fn reads_windows_line_ends_and_a_missing_number() {
        let subtitles = parse("00:00:01,000 --> 00:00:02,000\r\nHello\r\n\r\n");
        assert_eq!(subtitles.text_at(1500), Some("Hello"));
    }

    #[test]
    fn accepts_a_dot_and_extra_position_text() {
        let subtitles = parse("00:00:01.000 --> 00:00:02.000 X1:10 X2:20\nHello\n");
        assert_eq!(subtitles.text_at(1500), Some("Hello"));
    }

    #[test]
    fn slovene_letters_survive_from_every_encoding() {
        const CP1250: &[u8] = b"00:00:01,000 --> 00:00:02,000\n\xC8ez \x9Aest \x9Eab\n";
        assert_eq!(
            Subtitles::parse(CP1250).unwrap().text_at(1500),
            Some("Čez šest žab")
        );

        let utf8 = "00:00:01,000 --> 00:00:02,000\nČez šest žab\n";
        assert_eq!(parse(utf8).text_at(1500), Some("Čez šest žab"));

        let with_bom = [&UTF8_BOM[..], utf8.as_bytes()].concat();
        assert_eq!(
            Subtitles::parse(&with_bom).unwrap().text_at(1500),
            Some("Čez šest žab")
        );

        let mut utf16 = UTF16_LE_BOM.to_vec();
        utf16.extend(utf8.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(
            Subtitles::parse(&utf16).unwrap().text_at(1500),
            Some("Čez šest žab")
        );
    }

    #[test]
    fn removes_formatting_tags() {
        let subtitles = parse(
            "00:00:01,000 --> 00:00:02,000\n{\\an8}<i>Hello</i> <font color=\"red\">you</font>\n",
        );
        assert_eq!(subtitles.text_at(1500), Some("Hello you"));
    }

    #[test]
    fn a_comparison_is_not_a_tag() {
        let subtitles = parse("00:00:01,000 --> 00:00:02,000\n<i>5 < 6 {really}</i>\n");
        assert_eq!(subtitles.text_at(1500), Some("5 < 6 {really}"));
    }

    #[test]
    fn cues_out_of_order_and_overlapping_still_work() {
        let subtitles = parse(
            "00:00:05,000 --> 00:00:06,000\nLater\n\n\
             00:00:01,000 --> 00:00:10,000\nLong\n\n\
             00:00:02,000 --> 00:00:03,000\nShort\n\n",
        );
        assert_eq!(subtitles.text_at(2500), Some("Short"));
        assert_eq!(subtitles.text_at(5500), Some("Later"));
        assert_eq!(subtitles.text_at(8000), Some("Long"));
    }

    #[test]
    fn damaged_parts_are_skipped() {
        let subtitles = parse(
            "garbage\n99:99:99,999 --> nonsense\nLost\n\n\
             00:00:09,000 --> 00:00:08,000\nBackwards\n\n\
             00:61:00,000 --> 00:62:00,000\nBad minutes\n\n\
             00:00:01,000 --> 00:00:02,000\nGood\n\n\
             99999999999999:00:00,000 --> 99999999999999:00:01,000\nHuge\n\n",
        );
        assert_eq!(subtitles.cues.len(), 1);
        assert_eq!(subtitles.text_at(1500), Some("Good"));
    }

    #[test]
    fn a_file_without_subtitles_is_an_error() {
        for bytes in [&b""[..], b"\xFF\xFE\xFF", b"nonsense\n\n", &[0; 64]] {
            assert!(matches!(Subtitles::parse(bytes), Err(SubtitleError::Empty)));
        }
    }

    #[test]
    fn limits_are_kept() {
        let long = "x".repeat(MAX_CUE_CHARACTERS * 2);
        let subtitles = parse(&format!("00:00:01,000 --> 00:00:02,000\n{long}\n"));
        assert_eq!(subtitles.cues[0].text.len(), MAX_CUE_CHARACTERS);

        let cue = "00:00:01,000 --> 00:00:02,000\nA\n\n";
        assert_eq!(parse(&cue.repeat(MAX_CUES + 10)).cues.len(), MAX_CUES);
    }
}
