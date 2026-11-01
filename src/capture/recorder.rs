//! Records a screen region to an MPEG-TS file while producing preview frames.
//!
//! MPEG-TS is the container Miracast streams use, and a file cut short by a
//! crash stays playable.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use gst::glib;
use gst::prelude::*;
use thiserror::Error;
use tracing::{debug, info, warn};

use super::monitor::CaptureRegion;

const FRAME_RATE: u32 = 30;
const PREVIEW_FRAME_RATE: u32 = 10;
const PREVIEW_MAX_WIDTH: i64 = 480;
const PREVIEW_MAX_HEIGHT: i64 = 270;
const PREVIEW_BYTES_PER_PIXEL: usize = 4;
const PROBE_TIMEOUT_SECONDS: u64 = 3;
/// How long the pipeline may take to flush the file after a stop request.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// An H.264 encoder. Follow it with caps that request Constrained Baseline, as
/// Miracast requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encoder {
    name: &'static str,
    description: &'static str,
}

/// Forces one encoder by element name, to tell encoder problems from others.
const ENCODER_OVERRIDE_VARIABLE: &str = "SCREENCOPY_ENCODER";

/// In order of preference. The older `nvh264enc` is left out on purpose:
/// its presets are rejected by current NVIDIA drivers.
const ENCODERS: [Encoder; 2] = [
    Encoder {
        name: "nvcudah264enc",
        description: "nvcudah264enc preset=p1 tune=ultra-low-latency rate-control=cbr \
                      bitrate=8000 gop-size=30 b-frames=0 aud=true",
    },
    Encoder {
        name: "x264enc",
        description: "x264enc tune=zerolatency speed-preset=ultrafast bitrate=8000 \
                      key-int-max=30",
    },
];

impl Encoder {
    /// Returns the first encoder that can actually encode on this computer.
    /// An installed element proves little: NVENC also needs a working driver.
    /// The first call blocks for a moment; the result is then kept.
    pub fn detect() -> Option<Self> {
        static DETECTED: OnceLock<Option<Encoder>> = OnceLock::new();
        *DETECTED.get_or_init(|| {
            let forced = std::env::var(ENCODER_OVERRIDE_VARIABLE).ok();
            ENCODERS
                .into_iter()
                .filter(|encoder| forced.as_deref().is_none_or(|name| name == encoder.name))
                .find(Self::works)
        })
    }

    /// The encoder that needs no particular hardware.
    pub fn software() -> Self {
        ENCODERS[ENCODERS.len() - 1]
    }

    pub fn description(&self) -> &'static str {
        self.description
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    fn works(&self) -> bool {
        let description = format!(
            "videotestsrc num-buffers=1 ! video/x-raw,width=640,height=360,format=NV12 \
             ! {} ! video/x-h264,profile=constrained-baseline ! fakesink",
            self.description
        );
        let usable = probe(&description);
        debug!(encoder = self.name, usable, "probed encoder");
        usable
    }
}

fn probe(description: &str) -> bool {
    let Ok(pipeline) = gst::parse::launch(description) else {
        return false;
    };
    let usable = pipeline.set_state(gst::State::Playing).is_ok()
        && pipeline.bus().is_some_and(|bus| {
            bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(PROBE_TIMEOUT_SECONDS),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .is_some_and(|message| message.type_() == gst::MessageType::Eos)
        });
    let _ = pipeline.set_state(gst::State::Null);
    usable
}

#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("cannot create the capture pipeline: {0}")]
    Parse(#[from] glib::Error),
    #[error("the capture pipeline has an unexpected structure")]
    Structure,
    #[error("cannot start the capture pipeline: {0}")]
    Start(#[from] gst::StateChangeError),
    #[error("cannot watch the capture pipeline: {0}")]
    Watch(#[from] glib::BoolError),
}

#[derive(Debug)]
pub enum RecorderEvent {
    /// The file was completely written.
    Finished,
    Failed(String),
}

/// One RGBA preview image.
pub struct PreviewFrame {
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub data: glib::Bytes,
}

/// A running recording. Dropping it stops the capture immediately.
pub struct Recorder {
    pipeline: gst::Pipeline,
    preview: gst_app::AppSink,
    preview_size: (i32, i32),
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Recorder {
    /// Starts capturing `region`. Must be called on the GTK main thread.
    /// `on_event` runs on that thread once the recording has ended.
    pub fn start(
        region: CaptureRegion,
        path: &Path,
        encoder: Encoder,
        on_event: impl Fn(RecorderEvent) + 'static,
    ) -> Result<Self, RecorderError> {
        let region = region.with_even_size();
        info!(encoder = encoder.name, %region, "starting capture");
        Self::with_source(
            &screen_source(region),
            preview_size(region.width(), region.height()),
            path,
            encoder,
            on_event,
        )
    }

    fn with_source(
        source: &str,
        preview_size: (i32, i32),
        path: &Path,
        encoder: Encoder,
        on_event: impl Fn(RecorderEvent) + 'static,
    ) -> Result<Self, RecorderError> {
        let (pipeline, preview) = build_pipeline(source, preview_size, path, encoder)?;
        let bus = pipeline.bus().ok_or(RecorderError::Structure)?;
        pipeline.set_state(gst::State::Playing)?;

        // Messages posted before this point wait on the bus.
        let on_event = Rc::new(RefCell::new(Some(on_event)));
        let bus_watch = bus.add_watch_local(move |_, message| {
            let event = match message.view() {
                gst::MessageView::Eos(_) => Some(RecorderEvent::Finished),
                gst::MessageView::Error(error) => {
                    warn!(error = %error.error(), debug = ?error.debug(), "pipeline error");
                    Some(RecorderEvent::Failed(error.error().to_string()))
                }
                _ => None,
            };
            if let Some(event) = event
                && let Some(on_event) = on_event.borrow_mut().take()
            {
                on_event(event);
            }
            glib::ControlFlow::Continue
        })?;

        Ok(Self {
            pipeline,
            preview,
            preview_size,
            _bus_watch: bus_watch,
        })
    }

    /// Asks the pipeline to finish the file. The result arrives through the
    /// event callback, as a failure if the pipeline does not finish in time.
    pub fn stop(&self) {
        debug!("stopping capture");
        self.pipeline.send_event(gst::event::Eos::new());
        let pipeline = self.pipeline.downgrade();
        glib::timeout_add_local_once(STOP_TIMEOUT, move || {
            if let Some(pipeline) = pipeline.upgrade() {
                let message = gst::message::Error::builder(
                    gst::CoreError::Failed,
                    "Capture did not stop in time",
                )
                .build();
                if pipeline.post_message(message).is_err() {
                    warn!("cannot report the stop timeout");
                }
            }
        });
    }

    /// The newest preview image, if a new one is ready. Does not block.
    pub fn pull_preview(&self) -> Option<PreviewFrame> {
        let sample = self.preview.try_pull_sample(gst::ClockTime::ZERO)?;
        let buffer = sample.buffer()?;
        let map = buffer.map_readable().ok()?;
        let (width, height) = self.preview_size;
        let stride = usize::try_from(width).ok()? * PREVIEW_BYTES_PER_PIXEL;
        if map.len() < stride * usize::try_from(height).ok()? {
            warn!(len = map.len(), "preview buffer is too small");
            return None;
        }
        Some(PreviewFrame {
            width,
            height,
            stride,
            data: glib::Bytes::from(map.as_slice()),
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Err(error) = self.pipeline.set_state(gst::State::Null) {
            warn!(%error, "cannot stop the capture pipeline");
        }
        debug!("capture stopped");
    }
}

pub(super) fn screen_source(region: CaptureRegion) -> String {
    format!(
        "ximagesrc use-damage=false startx={} starty={} endx={} endy={}",
        region.start_x, region.start_y, region.end_x, region.end_y
    )
}

/// Fits the size into the preview box, keeping the aspect ratio.
fn preview_size(width: u32, height: u32) -> (i32, i32) {
    let (width, height) = (i64::from(width).max(1), i64::from(height).max(1));
    let (preview_width, preview_height) =
        if width * PREVIEW_MAX_HEIGHT >= height * PREVIEW_MAX_WIDTH {
            (PREVIEW_MAX_WIDTH, height * PREVIEW_MAX_WIDTH / width)
        } else {
            (width * PREVIEW_MAX_HEIGHT / height, PREVIEW_MAX_HEIGHT)
        };
    // The box limits make these casts lossless.
    (preview_width.max(2) as i32, preview_height.max(2) as i32)
}

fn pipeline_description(source: &str, preview_size: (i32, i32), encoder: Encoder) -> String {
    let (preview_width, preview_height) = preview_size;
    format!(
        "{source} ! video/x-raw,framerate={FRAME_RATE}/1 ! tee name=split \
         split. ! queue ! videoconvert ! video/x-raw,format=NV12 \
         ! {encoder} ! video/x-h264,profile=constrained-baseline \
         ! h264parse config-interval=-1 ! mpegtsmux \
         ! filesink name=output \
         split. ! queue leaky=downstream max-size-buffers=1 \
         ! videorate ! videoscale ! videoconvert \
         ! video/x-raw,format=RGBA,width={preview_width},height={preview_height},\
         pixel-aspect-ratio=1/1,framerate={PREVIEW_FRAME_RATE}/1 \
         ! appsink name=preview drop=true max-buffers=1 sync=false wait-on-eos=false",
        encoder = encoder.description,
    )
}

fn build_pipeline(
    source: &str,
    preview_size: (i32, i32),
    path: &Path,
    encoder: Encoder,
) -> Result<(gst::Pipeline, gst_app::AppSink), RecorderError> {
    let description = pipeline_description(source, preview_size, encoder);
    debug!(%description, "building pipeline");
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| RecorderError::Structure)?;
    // The path is user input, so it is never part of the description.
    pipeline
        .by_name("output")
        .ok_or(RecorderError::Structure)?
        .set_property("location", path);
    let preview = pipeline
        .by_name("preview")
        .ok_or(RecorderError::Structure)?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| RecorderError::Structure)?;
    Ok((pipeline, preview))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::*;

    const X264: Encoder = ENCODERS[1];
    const TEST_SIZE: (i32, i32) = (160, 90);
    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    fn test_source(frames: u32) -> String {
        format!(
            "videotestsrc is-live=true num-buffers={frames} \
             ! video/x-raw,format=BGRx,width=640,height=360 ! identity"
        )
    }

    /// A unique file in the system temporary directory, removed on drop.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let name = format!("screencopy-{}-{name}.ts", std::process::id());
            Self(std::env::temp_dir().join(name))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Reads back the width, height and profile of the H.264 stream in a file.
    fn read_video_info(path: &Path) -> (i32, i32, String) {
        let pipeline =
            gst::parse::launch("filesrc name=input ! tsdemux ! h264parse ! fakesink name=sink")
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
        pipeline
            .by_name("input")
            .unwrap()
            .set_property("location", path);
        pipeline.set_state(gst::State::Paused).unwrap();
        let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(10));
        result.unwrap();
        let caps = pipeline
            .by_name("sink")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .current_caps()
            .unwrap();
        let info = caps.structure(0).unwrap();
        let result = (
            info.get("width").unwrap(),
            info.get("height").unwrap(),
            info.get::<String>("profile").unwrap(),
        );
        pipeline.set_state(gst::State::Null).unwrap();
        result
    }

    fn assert_records_valid_stream(encoder: Encoder) {
        let file = TempFile::new(encoder.name);
        let (pipeline, preview) =
            build_pipeline(&test_source(30), TEST_SIZE, &file.0, encoder).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(30),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");

        let sample = preview.try_pull_sample(gst::ClockTime::ZERO).unwrap();
        let caps = sample.caps().unwrap().structure(0).unwrap().to_owned();
        assert_eq!(caps.get::<i32>("width").unwrap(), TEST_SIZE.0);
        assert_eq!(caps.get::<i32>("height").unwrap(), TEST_SIZE.1);
        pipeline.set_state(gst::State::Null).unwrap();

        // Every MPEG-TS packet starts with the sync byte 0x47.
        let contents = std::fs::read(&file.0).unwrap();
        assert!(!contents.is_empty());
        assert_eq!(contents.len() % 188, 0);
        assert!(contents.chunks(188).all(|packet| packet[0] == 0x47));

        let (width, height, profile) = read_video_info(&file.0);
        assert_eq!((width, height), (640, 360));
        assert_eq!(profile, "constrained-baseline");
    }

    #[test]
    fn x264_records_constrained_baseline_mpeg_ts() {
        gst::init().unwrap();
        assert_records_valid_stream(X264);
    }

    #[test]
    fn detected_encoder_records_constrained_baseline_mpeg_ts() {
        gst::init().unwrap();
        assert_records_valid_stream(Encoder::detect().unwrap());
    }

    #[test]
    fn missing_encoder_is_not_usable() {
        gst::init().unwrap();
        let encoder = Encoder {
            name: "missing",
            description: "this-element-does-not-exist",
        };
        assert!(!encoder.works());
    }

    /// Runs the GLib main loop until the recording reports its end, stopping
    /// it after a moment, and checks that it finished cleanly.
    #[test]
    fn stop_finishes_the_file() {
        gst::init().unwrap();
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let file = TempFile::new("stop");

        let event = Rc::new(Cell::new(None));
        let recorder = {
            let event = Rc::clone(&event);
            Recorder::with_source(&test_source(100_000), TEST_SIZE, &file.0, X264, move |e| {
                event.set(Some(e));
            })
            .unwrap()
        };
        let started = std::time::Instant::now();
        let mut stop_requested = false;
        let result = loop {
            context.iteration(false);
            if let Some(result) = event.take() {
                break result;
            }
            if !stop_requested && recorder.pull_preview().is_some() {
                recorder.stop();
                stop_requested = true;
            }
            assert!(started.elapsed() < TEST_TIMEOUT, "recording did not end");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(result, RecorderEvent::Finished), "{result:?}");
        drop(recorder);

        let (width, height, _) = read_video_info(&file.0);
        assert_eq!((width, height), (640, 360));
    }

    #[test]
    fn unwritable_path_is_an_error() {
        gst::init().unwrap();
        let path = Path::new("/nonexistent-screencopy-directory/test.ts");
        let result = Recorder::with_source(&test_source(1), TEST_SIZE, path, X264, |_| {});
        assert!(matches!(result, Err(RecorderError::Start(_))));
    }

    #[test]
    fn invalid_source_is_an_error() {
        gst::init().unwrap();
        let path = Path::new("unused.ts");
        let result = Recorder::with_source("no-such-element", TEST_SIZE, path, X264, |_| {});
        assert!(matches!(result, Err(RecorderError::Parse(_))));
    }

    #[test]
    fn preview_fits_landscape_and_portrait() {
        assert_eq!(preview_size(2560, 1440), (480, 270));
        assert_eq!(preview_size(1080, 1920), (151, 270));
        assert_eq!(preview_size(1920, 1080), (480, 270));
    }

    #[test]
    fn preview_size_survives_extreme_shapes() {
        assert_eq!(preview_size(65535, 1), (480, 2));
        assert_eq!(preview_size(1, 65535), (2, 270));
    }

    #[test]
    fn screen_source_uses_region_bounds() {
        let region = CaptureRegion {
            start_x: 2560,
            start_y: 0,
            end_x: 3639,
            end_y: 1919,
        };
        assert_eq!(
            screen_source(region),
            "ximagesrc use-damage=false startx=2560 starty=0 endx=3639 endy=1919"
        );
    }
}
