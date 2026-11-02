//! Wi-Fi Display parameters: reads what the TV supports and chooses what to
//! send. The text comes from the TV and is untrusted.

use std::collections::HashMap;

const MAX_PARAMETERS: usize = 64;
const PROFILE_CONSTRAINED_BASELINE: u8 = 0x01;
const NONE: &str = "none";
/// Asks for a frame rate, to compare smoothness on a particular TV and network.
const FRAME_RATE_VARIABLE: &str = "SCREENCOPY_FRAME_RATE";

/// H.264 levels in the bit positions the specification gives them.
const LEVEL_3_1: u8 = 0x01;
const LEVEL_3_2: u8 = 0x02;
const LEVEL_4: u8 = 0x04;
const LEVEL_4_2: u8 = 0x10;

/// A video format the TV can be asked to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
    /// The format's single bit in the CEA table.
    cea_bit: u32,
    level: u8,
}

impl VideoMode {
    pub fn level_name(&self) -> &'static str {
        match self.level {
            LEVEL_3_1 => "3.1",
            LEVEL_3_2 => "3.2",
            LEVEL_4 => "4",
            _ => "4.2",
        }
    }

    pub fn to_parameter(self) -> String {
        format!(
            "00 00 {PROFILE_CONSTRAINED_BASELINE:02x} {:02x} {:08x} 00000000 00000000 00 0000 0000 00 none none",
            self.level, self.cea_bit
        )
    }
}

const fn mode(cea_index: u32, width: u32, height: u32, frame_rate: u32, level: u8) -> VideoMode {
    VideoMode {
        width,
        height,
        frame_rate,
        cea_bit: 1 << cea_index,
        level,
    }
}

/// In order of preference. 30 frames per second comes first, as it is far kinder
/// to Wi-Fi and to the encoder, and is plenty for mirroring a desktop.
const MODES: [VideoMode; 5] = [
    mode(7, 1920, 1080, 30, LEVEL_4),
    mode(8, 1920, 1080, 60, LEVEL_4_2),
    mode(5, 1280, 720, 30, LEVEL_3_1),
    mode(6, 1280, 720, 60, LEVEL_3_2),
    mode(0, 640, 480, 60, LEVEL_3_1),
];

/// Splits a `key: value` body. Keys are made lower case.
pub fn parse_parameters(body: &str) -> HashMap<String, String> {
    body.lines()
        .take(MAX_PARAMETERS)
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect()
}

/// One H.264 capability the TV listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct H264Support {
    profile: u8,
    level: u8,
    cea: u32,
}

/// Reads `wfd_video_formats`: two fields for the native and preferred display
/// modes, then comma-separated entries that each start with profile, level and
/// CEA table. Invalid entries are skipped.
fn parse_video_formats(value: &str) -> Vec<H264Support> {
    let mut entries = value.split(',');
    let Some(first) = entries.next() else {
        return Vec::new();
    };
    let first = first
        .split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ");
    std::iter::once(first.as_str())
        .chain(entries)
        .filter_map(|entry| {
            let mut fields = entry.split_whitespace();
            Some(H264Support {
                profile: u8::from_str_radix(fields.next()?, 16).ok()?,
                level: u8::from_str_radix(fields.next()?, 16).ok()?,
                cea: u32::from_str_radix(fields.next()?, 16).ok()?,
            })
        })
        .collect()
}

/// The best format that the TV supports in Constrained Baseline, if any.
pub fn choose_video_mode(video_formats: &str) -> Option<VideoMode> {
    let preferred_rate = std::env::var(FRAME_RATE_VARIABLE)
        .ok()
        .and_then(|value| value.parse().ok());
    choose_video_mode_at_rate(video_formats, preferred_rate)
}

/// Prefers `preferred_rate` where the TV offers it, then the usual order.
fn choose_video_mode_at_rate(
    video_formats: &str,
    preferred_rate: Option<u32>,
) -> Option<VideoMode> {
    let support: Vec<H264Support> = parse_video_formats(video_formats)
        .into_iter()
        .filter(|entry| entry.profile & PROFILE_CONSTRAINED_BASELINE != 0)
        .collect();
    let supported = |mode: &VideoMode| {
        support
            .iter()
            .any(|entry| entry.cea & mode.cea_bit != 0 && entry.level >= mode.level)
    };
    MODES
        .into_iter()
        .find(|mode| Some(mode.frame_rate) == preferred_rate && supported(mode))
        .or_else(|| MODES.into_iter().find(|mode| supported(mode)))
}

/// Whether the TV lists AAC among its audio formats.
pub fn supports_aac(audio_codecs: &str) -> bool {
    audio_codecs
        .split(',')
        .any(|entry| entry.split_whitespace().next() == Some("AAC"))
}

/// Reads the RTP port from `wfd_client_rtp_ports`, such as
/// `RTP/AVP/UDP;unicast 1028 0 mode=play`.
pub fn parse_client_rtp_port(value: &str) -> Option<u16> {
    if value == NONE {
        return None;
    }
    let port: u16 = value.split_whitespace().nth(1)?.parse().ok()?;
    (port != 0).then_some(port)
}

/// Reads the port from a `Transport` header's `client_port=1028` or `=1028-1029`.
pub fn parse_transport_client_port(value: &str) -> Option<u16> {
    let (_, ports) = value
        .split(';')
        .find_map(|part| part.trim().split_once("client_port="))?;
    let port: u16 = ports.split('-').next()?.parse().ok()?;
    (port != 0).then_some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATS_1080P30_ONLY: &str =
        "00 00 01 04 00000080 00000000 00000000 00 0000 0000 00 none none";
    const TYPICAL_TV: &str = "00 00 02 04 000184ff 00000000 00000000 00 0000 0000 11 none none, \
                              01 04 000184ff 00000000 00000000 00 0000 0000 11 none none";

    #[test]
    fn prefers_1080p30() {
        let chosen = choose_video_mode(TYPICAL_TV).unwrap();
        assert_eq!(
            (chosen.width, chosen.height, chosen.frame_rate),
            (1920, 1080, 30)
        );
        assert_eq!(chosen.level_name(), "4");
    }

    #[test]
    fn prefers_the_requested_frame_rate_on_a_real_tv() {
        // As reported by a Samsung AU7172 TV.
        let formats = "40 00 01 10 000001e3 0f3fffff 00000fff 00 0000 00c8 01 none none";
        let usual = choose_video_mode_at_rate(formats, None).unwrap();
        assert_eq!((usual.height, usual.frame_rate), (1080, 30));
        let smooth = choose_video_mode_at_rate(formats, Some(60)).unwrap();
        assert_eq!((smooth.height, smooth.frame_rate), (1080, 60));
        assert_eq!(smooth.level_name(), "4.2");
    }

    #[test]
    fn unavailable_frame_rate_falls_back_to_the_usual_order() {
        let chosen = choose_video_mode_at_rate(FORMATS_1080P30_ONLY, Some(60)).unwrap();
        assert_eq!((chosen.height, chosen.frame_rate), (1080, 30));
    }

    #[test]
    fn uses_a_lower_mode_when_the_level_is_too_low() {
        let formats = "00 00 01 01 000000e0 00000000 00000000 00 0000 0000 00 none none";
        let chosen = choose_video_mode(formats).unwrap();
        assert_eq!((chosen.width, chosen.frame_rate), (1280, 30));
    }

    #[test]
    fn requires_constrained_baseline() {
        let formats = "00 00 02 10 000184ff 00000000 00000000 00 0000 0000 00 none none";
        assert_eq!(choose_video_mode(formats), None);
    }

    #[test]
    fn finds_constrained_baseline_in_a_later_entry() {
        let formats = "00 00 02 10 00000001 00000000 00000000 00 0000 0000 00 none none, \
                       01 04 00000080 00000000 00000000 00 0000 0000 00 none none";
        assert_eq!(choose_video_mode(formats).unwrap().height, 1080);
    }

    #[test]
    fn unusable_formats_choose_nothing() {
        for formats in [
            "",
            "none",
            "00 00",
            "00 00 zz zz zz",
            "00 00 01 04 00000000",
            ",,,",
        ] {
            assert_eq!(choose_video_mode(formats), None, "{formats:?}");
        }
    }

    #[test]
    fn chosen_mode_describes_exactly_one_format() {
        assert_eq!(
            MODES[0].to_parameter(),
            "00 00 01 04 00000080 00000000 00000000 00 0000 0000 00 none none"
        );
    }

    #[test]
    fn a_tv_accepts_its_own_mode_back() {
        for mode in MODES {
            assert_eq!(choose_video_mode(&mode.to_parameter()), Some(mode));
        }
    }

    #[test]
    fn splits_parameters() {
        let parameters =
            parse_parameters("wfd_audio_codecs: AAC 00000001 00\r\nWFD_X:y z\r\nbad\r\n");
        assert_eq!(parameters["wfd_audio_codecs"], "AAC 00000001 00");
        assert_eq!(parameters["wfd_x"], "y z");
        assert_eq!(parameters.len(), 2);
    }

    #[test]
    fn limits_the_number_of_parameters() {
        let body = (0..1000)
            .map(|i| format!("k{i}: v\r\n"))
            .collect::<String>();
        assert_eq!(parse_parameters(&body).len(), MAX_PARAMETERS);
    }

    #[test]
    fn recognizes_aac() {
        assert!(supports_aac("LPCM 00000003 00, AAC 00000001 00"));
        assert!(!supports_aac("LPCM 00000003 00"));
        assert!(!supports_aac("none"));
    }

    #[test]
    fn reads_rtp_ports() {
        assert_eq!(
            parse_client_rtp_port("RTP/AVP/UDP;unicast 1028 0 mode=play"),
            Some(1028)
        );
        for value in [
            "none",
            "",
            "RTP",
            "RTP/AVP/UDP;unicast 0 0",
            "RTP x",
            "RTP 70000 0",
        ] {
            assert_eq!(parse_client_rtp_port(value), None, "{value:?}");
        }
    }

    #[test]
    fn reads_transport_ports() {
        let read = parse_transport_client_port;
        assert_eq!(read("RTP/AVP/UDP;unicast;client_port=1028"), Some(1028));
        assert_eq!(
            read("RTP/AVP/UDP;client_port=1028-1029;unicast"),
            Some(1028)
        );
        assert_eq!(read("RTP/AVP/UDP;unicast"), None);
        assert_eq!(read("client_port=0"), None);
        assert_eq!(read("client_port=x"), None);
        assert_eq!(read("client_port=99999"), None);
    }
}
