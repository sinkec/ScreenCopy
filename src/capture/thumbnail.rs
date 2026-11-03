//! One small picture of a monitor, so that the user can recognize it. It stays
//! in memory, on this computer: it is never saved or sent anywhere.

use gst::prelude::*;

use super::monitor::CaptureRegion;
use super::recorder::screen_source;

const CAPTURE_TIMEOUT_SECONDS: u64 = 3;
const BYTES_PER_PIXEL: usize = 4;

/// An image with 8-bit red, green and blue, and one unused byte per pixel. It
/// has no alpha channel on purpose: the screen has none, and converting it to a
/// format with one leaves every pixel fully transparent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub pixels: Vec<u8>,
}

/// Takes one picture of `region`. Blocks for a moment, so do not call it on the
/// GTK thread. `None` means no picture, which the user can do without.
pub fn capture(region: CaptureRegion, width: i32, height: i32) -> Option<Thumbnail> {
    from_source(&screen_source(region), width, height)
}

fn picture_description(source: &str, width: i32, height: i32) -> String {
    format!(
        "{source} num-buffers=1 ! videoconvert ! videoscale \
         ! video/x-raw,format=RGBx,width={width},height={height},pixel-aspect-ratio=1/1 \
         ! appsink name=picture sync=false"
    )
}

fn from_source(source: &str, width: i32, height: i32) -> Option<Thumbnail> {
    let description = picture_description(source, width, height);
    let pipeline = gst::parse::launch(&description)
        .inspect_err(|error| tracing::debug!(%error, "cannot build the picture pipeline"))
        .ok()?
        .downcast::<gst::Bin>()
        .ok()?;
    let sink = pipeline
        .by_name("picture")?
        .downcast::<gst_app::AppSink>()
        .ok()?;
    pipeline.set_state(gst::State::Playing).ok()?;
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(CAPTURE_TIMEOUT_SECONDS));
    let _ = pipeline.set_state(gst::State::Null);

    let sample = sample?;
    let map = sample.buffer()?.map_readable().ok()?;
    let stride = usize::try_from(width).ok()? * BYTES_PER_PIXEL;
    if map.len() < stride * usize::try_from(height).ok()? {
        return None;
    }
    Some(Thumbnail {
        width,
        height,
        stride,
        pixels: map.as_slice().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_a_picture_of_the_requested_size() {
        gst::init().unwrap();
        let source = "videotestsrc pattern=ball";
        let thumbnail = from_source(source, 90, 160).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (90, 160));
        assert_eq!(thumbnail.stride, 360);
        assert!(thumbnail.pixels.len() >= 360 * 160);
    }

    #[test]
    fn a_picture_has_real_content() {
        gst::init().unwrap();
        let thumbnail = from_source("videotestsrc pattern=smpte", 64, 36).unwrap();
        let green: std::collections::HashSet<u8> = thumbnail
            .pixels
            .iter()
            .skip(1)
            .step_by(4)
            .copied()
            .collect();
        assert!(green.len() > 1, "a flat picture: {green:?}");
    }

    #[test]
    fn pictures_are_requested_without_an_alpha_channel() {
        assert!(picture_description("x", 1, 1).contains("format=RGBx"));
    }

    #[test]
    fn an_impossible_source_means_no_picture() {
        gst::init().unwrap();
        assert_eq!(from_source("no-such-element", 90, 160), None);
    }
}
