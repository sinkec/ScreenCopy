//! Sends a screen region to a TV as H.264 in MPEG-TS over RTP.

use std::net::Ipv4Addr;

use gst::prelude::*;
use tracing::{debug, warn};

use super::monitor::CaptureRegion;
use super::recorder::{Encoder, screen_source};
use crate::audio::{AudioRoute, MONITOR_SOURCE};
use crate::wfd::session::{MediaStream, Sound, StreamTarget};

/// Silence, for when the computer's sound cannot be captured: some TVs expect
/// a stream to carry audio.
const SILENT_AUDIO_SOURCE: &str = "audiotestsrc is-live=true wave=silence";
const AAC_ENCODER: &str = "avenc_aac";

pub struct ScreenStream {
    source: String,
    encoder: Encoder,
    local_address: Ipv4Addr,
    use_system_audio: bool,
    audio_route: Option<AudioRoute>,
    pipeline: Option<gst::Element>,
}

impl ScreenStream {
    pub fn new(region: CaptureRegion, encoder: Encoder, local_address: Ipv4Addr) -> Self {
        let mut stream = Self::with_source(
            screen_source(region.with_even_size()),
            encoder,
            local_address,
        );
        stream.use_system_audio = true;
        stream
    }

    fn with_source(source: String, encoder: Encoder, local_address: Ipv4Addr) -> Self {
        Self {
            source,
            encoder,
            local_address,
            use_system_audio: false,
            audio_route: None,
            pipeline: None,
        }
    }

    /// Where the audio comes from. Sharing continues silently if the system's
    /// sound cannot be captured, which is better than no picture.
    fn audio_source(&mut self) -> String {
        if self.use_system_audio {
            match AudioRoute::activate() {
                Ok(route) => {
                    self.audio_route = Some(route);
                    return system_audio_source();
                }
                Err(error) => warn!(%error, "sending silence: cannot capture the sound"),
            }
        }
        SILENT_AUDIO_SOURCE.to_owned()
    }
}

fn system_audio_source() -> String {
    format!(
        "pulsesrc device={MONITOR_SOURCE} client-name=ScreenCopy buffer-time=100000 \
         latency-time=10000 provide-clock=false do-timestamp=true"
    )
}

fn audio_description(source: &str) -> String {
    format!(
        "{source} ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! audioresample \
         ! avenc_aac bitrate=128000 ! aacparse ! audio/mpeg,stream-format=adts ! queue \
         ! mux.sink_4352"
    )
}

/// The muxer's pad names set the packet IDs. The Wi-Fi Display specification
/// requires 0x1011 (4113) for video and 0x1100 (4352) for audio, and a TV may not
/// look anywhere else.
///
/// Contains only numbers and addresses in their typed form, and nothing from the
/// TV as text, so it cannot be used to alter the pipeline.
fn stream_description(
    source: &str,
    encoder: Encoder,
    local_address: Ipv4Addr,
    target: StreamTarget,
    audio_source: Option<&str>,
) -> String {
    let StreamTarget {
        address,
        port,
        local_port,
        mode,
        ..
    } = target;
    let audio = audio_source.map(audio_description).unwrap_or_default();
    format!(
        "{source} ! video/x-raw,framerate={rate}/1 ! queue leaky=downstream max-size-buffers=2 \
         ! videoscale add-borders=true ! videoconvert \
         ! video/x-raw,format=NV12,width={width},height={height},pixel-aspect-ratio=1/1 \
         ! {encoder} ! video/x-h264,profile=constrained-baseline,level=(string){level} \
         ! h264parse config-interval=-1 ! queue ! mux.sink_4113 {audio} \
         mpegtsmux name=mux alignment=7 \
         ! rtpmp2tpay ssrc=1 timestamp-offset=0 seqnum-offset=0 perfect-rtptime=false \
         ! udpsink host={address} port={port} bind-address={local_address} \
         bind-port={local_port} sync=false",
        rate = mode.frame_rate,
        width = mode.width,
        height = mode.height,
        encoder = encoder.description(),
        level = mode.level_name(),
    )
}

impl MediaStream for ScreenStream {
    fn start(&mut self, target: StreamTarget) -> Result<Sound, String> {
        let audio_source = (target.with_audio && gst::ElementFactory::find(AAC_ENCODER).is_some())
            .then(|| self.audio_source());
        let description = stream_description(
            &self.source,
            self.encoder,
            self.local_address,
            target,
            audio_source.as_deref(),
        );
        debug!(%description, "building stream pipeline");
        let pipeline = gst::parse::launch(&description).map_err(|error| error.to_string())?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        self.pipeline = Some(pipeline);
        Ok(match (&audio_source, &self.audio_route) {
            (None, _) => Sound::Absent,
            (Some(_), None) => Sound::Silent,
            (Some(_), Some(_)) => Sound::System,
        })
    }

    fn poll_failure(&mut self) -> Option<String> {
        if let Some(route) = &mut self.audio_route {
            route.refresh();
        }
        let bus = self.pipeline.as_ref()?.bus()?;
        let message = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])?;
        match message.view() {
            gst::MessageView::Error(error) => {
                warn!(error = %error.error(), debug = ?error.debug(), "stream error");
                Some(error.error().to_string())
            }
            _ => Some("the capture ended unexpectedly".to_owned()),
        }
    }
}

impl Drop for ScreenStream {
    fn drop(&mut self) {
        if let Some(pipeline) = &self.pipeline
            && let Err(error) = pipeline.set_state(gst::State::Null)
        {
            warn!(%error, "cannot stop the stream pipeline");
        }
        // Only after capture has stopped, so that no sound is cut off or doubled.
        self.audio_route.take();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::wfd::params::choose_video_mode;

    const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;
    const FORMATS_1080P30: &str =
        "00 00 01 04 00000080 00000000 00000000 00 0000 0000 00 none none";
    const TEST_SOURCE: &str =
        "videotestsrc is-live=true ! video/x-raw,format=BGRx,width=1080,height=1920 ! identity";

    fn target(port: u16, with_audio: bool) -> StreamTarget {
        StreamTarget {
            address: LOCAL,
            port,
            local_port: 0,
            mode: choose_video_mode(FORMATS_1080P30).unwrap(),
            with_audio,
        }
    }

    /// Receives on a real UDP port what a TV would, and reads the video's
    /// properties from it.
    fn assert_stream_reaches_receiver(encoder: Encoder, with_audio: bool, port: u16) {
        gst::init().unwrap();
        let receiver = gst::parse::launch(&format!(
            "udpsrc address=127.0.0.1 port={port} \
             caps=application/x-rtp,media=video,clock-rate=90000,encoding-name=MP2T \
             ! rtpmp2tdepay ! tsdemux ! h264parse ! fakesink name=sink"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        receiver.set_state(gst::State::Playing).unwrap();

        let mut stream = ScreenStream::with_source(TEST_SOURCE.to_owned(), encoder, LOCAL);
        stream.start(target(port, with_audio)).unwrap();

        let sink_pad = receiver
            .by_name("sink")
            .unwrap()
            .static_pad("sink")
            .unwrap();
        let started = std::time::Instant::now();
        let caps = loop {
            assert_eq!(stream.poll_failure(), None);
            if let Some(caps) = sink_pad.current_caps() {
                break caps;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "no video arrived"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let info = caps.structure(0).unwrap();
        assert_eq!(info.get::<i32>("width").unwrap(), 1920);
        assert_eq!(info.get::<i32>("height").unwrap(), 1080);
        assert_eq!(
            info.get::<String>("profile").unwrap(),
            "constrained-baseline"
        );
        receiver.set_state(gst::State::Null).unwrap();
    }

    /// Reads real RTP packets from the socket a TV would use, and checks their
    /// headers and that the video and audio use the IDs the specification requires.
    #[test]
    fn packets_follow_the_wifi_display_specification() {
        gst::init().unwrap();
        let socket = std::net::UdpSocket::bind((LOCAL, 29315)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut stream =
            ScreenStream::with_source(TEST_SOURCE.to_owned(), Encoder::software(), LOCAL);
        stream.start(target(29315, true)).unwrap();

        const RTP_HEADER: usize = 12;
        const TS_PACKET: usize = 188;
        let mut packet_ids = std::collections::HashSet::new();
        let mut buffer = [0; 2048];
        for _ in 0..1000 {
            let length = socket.recv(&mut buffer).unwrap();
            assert_eq!(buffer[0] >> 6, 2, "RTP version");
            assert_eq!(buffer[1] & 0x7F, 33, "MPEG-TS payload type");
            assert_eq!(&buffer[8..12], &[0, 0, 0, 1], "SSRC");
            assert_eq!(length, RTP_HEADER + 7 * TS_PACKET);
            for ts in buffer[RTP_HEADER..length].chunks(TS_PACKET) {
                assert_eq!(ts[0], 0x47, "MPEG-TS sync byte");
                packet_ids.insert(u16::from_be_bytes([ts[1] & 0x1F, ts[2]]));
            }
            if packet_ids.contains(&0x1011) && packet_ids.contains(&0x1100) {
                return;
            }
        }
        panic!("missing video or audio packets: {packet_ids:x?}");
    }

    #[test]
    fn x264_stream_reaches_the_receiver() {
        assert_stream_reaches_receiver(Encoder::software(), false, 29311);
    }

    #[test]
    fn stream_with_audio_reaches_the_receiver() {
        assert_stream_reaches_receiver(Encoder::software(), true, 29312);
    }

    #[test]
    fn detected_encoder_stream_reaches_the_receiver() {
        gst::init().unwrap();
        assert_stream_reaches_receiver(Encoder::detect().unwrap(), true, 29313);
    }

    #[test]
    fn invalid_pipeline_is_an_error_not_a_panic() {
        gst::init().unwrap();
        let mut stream =
            ScreenStream::with_source("no-such-element".to_owned(), Encoder::software(), LOCAL);
        assert!(stream.start(target(29314, false)).is_err());
    }

    #[test]
    fn stream_goes_only_to_the_target_and_from_the_local_address() {
        gst::init().unwrap();
        let description = stream_description(
            TEST_SOURCE,
            Encoder::software(),
            LOCAL,
            target(1028, false),
            None,
        );
        assert!(description.contains("host=127.0.0.1 port=1028 bind-address=127.0.0.1"));
        assert!(!description.contains("audiotestsrc"));
    }

    #[test]
    fn system_audio_pipeline_is_valid_and_reads_only_the_virtual_output() {
        gst::init().unwrap();
        let source = system_audio_source();
        assert!(source.contains("device=screencopy_tv.monitor"));
        let description = stream_description(
            TEST_SOURCE,
            Encoder::software(),
            LOCAL,
            target(1028, true),
            Some(&source),
        );
        // Parsing builds every element and sets every property, without starting.
        gst::parse::launch(&description).unwrap();
    }

    #[test]
    fn a_real_screen_stream_uses_the_system_sound() {
        let region = CaptureRegion {
            start_x: 0,
            start_y: 0,
            end_x: 1919,
            end_y: 1079,
        };
        let stream = ScreenStream::new(region, Encoder::software(), LOCAL);
        assert!(stream.use_system_audio);
    }

    #[test]
    fn tests_never_touch_the_real_sound_system() {
        let stream = ScreenStream::with_source(TEST_SOURCE.to_owned(), Encoder::software(), LOCAL);
        assert!(!stream.use_system_audio);
    }

    #[test]
    fn nothing_runs_before_start() {
        let mut stream =
            ScreenStream::with_source(TEST_SOURCE.to_owned(), Encoder::software(), LOCAL);
        assert_eq!(stream.poll_failure(), None);
    }
}
