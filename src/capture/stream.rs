//! Sends a screen region to a TV as H.264 in MPEG-TS over RTP.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use gst::prelude::*;
use tracing::{debug, warn};

use super::monitor::CaptureRegion;
use super::recorder::{Encoder, screen_source};
use super::stream_stats::{StreamMeter, StreamStats};
use crate::audio::{AudioRoute, MONITOR_SOURCE};
use crate::wfd::session::{MediaStream, Sound, StreamTarget};

/// Silence, for when the computer's sound cannot be captured: some TVs expect
/// a stream to carry audio.
const SILENT_AUDIO_SOURCE: &str = "audiotestsrc is-live=true wave=silence";
const AAC_ENCODER: &str = "avenc_aac";
const KEY_FRAME_VARIABLE: &str = "SCREENCOPY_KEY_FRAME_DISTANCE";
const BITRATE_VARIABLE: &str = "SCREENCOPY_BITRATE_KBIT";
const PACING_VARIABLE: &str = "SCREENCOPY_PACING_MS";
/// Chosen on a real TV: visibly the smoothest of the settings tried.
const DEFAULT_PACING_MS: u32 = 100;
const CAPTURE_QUEUE: &str = "capture-queue";
const SENDER: &str = "sender";
const VIDEO_QUEUE: &str = "video-before-mux";
/// Often enough to catch a stall, rarely enough to keep the log readable.
const STATS_INTERVAL: Duration = Duration::from_secs(5);
const AUDIO_QUEUE: &str = "audio-before-mux";

pub struct ScreenStream {
    source: String,
    encoder: Encoder,
    local_address: Ipv4Addr,
    use_system_audio: bool,
    audio_route: Option<AudioRoute>,
    pipeline: Option<gst::Element>,
    /// Can change at any moment, because the user adjusts it while watching.
    sound_delay_ms: Arc<AtomicI32>,
    applied_sound_delay_ms: Option<i32>,
    meter: Option<StreamMeter>,
    stats_interval: Duration,
}

impl ScreenStream {
    pub fn new(
        region: CaptureRegion,
        encoder: Encoder,
        local_address: Ipv4Addr,
        sound_delay_ms: Arc<AtomicI32>,
    ) -> Self {
        let mut stream = Self::with_source(
            screen_source(region.with_even_size()),
            encoder,
            local_address,
        );
        stream.use_system_audio = true;
        stream.sound_delay_ms = sound_delay_ms;
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
            sound_delay_ms: Arc::default(),
            applied_sound_delay_ms: None,
            meter: None,
            stats_interval: STATS_INTERVAL,
        }
    }

    /// Applies a changed sound delay to the running pipeline. A positive delay
    /// moves the sound later, a negative one moves the picture later. Offsetting
    /// the pads changes the times the muxer writes into the stream, and with
    /// that, when the TV plays each part.
    fn apply_sound_delay(&mut self) {
        let delay = self.sound_delay_ms.load(Ordering::Relaxed);
        if self.applied_sound_delay_ms == Some(delay) {
            return;
        }
        let Some(pipeline) = self
            .pipeline
            .as_ref()
            .and_then(|pipeline| pipeline.downcast_ref::<gst::Bin>())
        else {
            return;
        };
        let (video_offset, audio_offset) = delay_offsets(delay);
        for (name, offset) in [(VIDEO_QUEUE, video_offset), (AUDIO_QUEUE, audio_offset)] {
            if let Some(pad) = pipeline
                .by_name(name)
                .and_then(|queue| queue.static_pad("src"))
            {
                pad.set_offset(offset);
            }
        }
        debug!(delay_ms = delay, "applied the sound delay");
        self.applied_sound_delay_ms = Some(delay);
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

/// Offsets in nanoseconds for the video and the audio. Only ever delays, because
/// a stream cannot be sent before it was captured.
fn delay_offsets(delay_ms: i32) -> (i64, i64) {
    const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
    let offset = i64::from(delay_ms) * NANOSECONDS_PER_MILLISECOND;
    (offset.min(0).abs(), offset.max(0))
}

/// Asks for a distance between key frames, to find out whether they are what
/// a TV stutters on.
fn key_frame_distance() -> Option<u32> {
    std::env::var(KEY_FRAME_VARIABLE).ok()?.parse().ok()
}

/// Asks for a bitrate in kbit/s, to find out whether the amount of data matters.
fn bitrate_override() -> Option<u32> {
    std::env::var(BITRATE_VARIABLE).ok()?.parse().ok()
}

/// How far behind real time packets are sent. Zero sends them as they come.
fn pacing_delay_ms() -> u32 {
    std::env::var(PACING_VARIABLE)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PACING_MS)
}

/// A TV shows uneven packets as stutter, and the encoder's output comes in
/// clumps, so packets leave by their timestamps. The delay gives late frames
/// time to catch up.
fn pacing_options(delay_ms: u32) -> String {
    if delay_ms == 0 {
        "sync=false".to_owned()
    } else {
        format!("sync=true ts-offset={}", u64::from(delay_ms) * 1_000_000)
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
         ! avenc_aac bitrate=128000 ! aacparse ! audio/mpeg,stream-format=adts \
         ! queue name={AUDIO_QUEUE} ! mux.sink_4352"
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
        "{source} ! video/x-raw,framerate={rate}/1 ! queue name={CAPTURE_QUEUE} leaky=downstream \
         max-size-buffers=2 \
         ! videoscale add-borders=true ! videoconvert \
         ! video/x-raw,format=NV12,width={width},height={height},pixel-aspect-ratio=1/1 \
         ! {encoder} ! video/x-h264,profile=constrained-baseline,level=(string){level} \
         ! h264parse config-interval=-1 ! queue name={VIDEO_QUEUE} ! mux.sink_4113 {audio} \
         mpegtsmux name=mux alignment=7 \
         ! rtpmp2tpay ssrc=1 timestamp-offset=0 seqnum-offset=0 perfect-rtptime=false \
         ! udpsink name={SENDER} host={address} port={port} bind-address={local_address} \
         bind-port={local_port} {pacing}",
        rate = mode.frame_rate,
        width = mode.width,
        height = mode.height,
        encoder = encoder.description_with(key_frame_distance(), bitrate_override()),
        level = mode.level_name(),
        pacing = pacing_options(pacing_delay_ms()),
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
        if let Some(bin) = pipeline.downcast_ref::<gst::Bin>() {
            self.meter = Some(StreamMeter::attach(
                bin,
                CAPTURE_QUEUE,
                VIDEO_QUEUE,
                SENDER,
                self.stats_interval,
            ));
        }
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        self.pipeline = Some(pipeline);
        self.apply_sound_delay();
        Ok(match (&audio_source, &self.audio_route) {
            (None, _) => Sound::Absent,
            (Some(_), None) => Sound::Silent,
            (Some(_), Some(_)) => Sound::System,
        })
    }

    fn poll_stats(&mut self) -> Option<StreamStats> {
        self.meter.as_mut()?.poll()
    }

    fn poll_failure(&mut self) -> Option<String> {
        if let Some(route) = &mut self.audio_route {
            route.refresh();
        }
        self.apply_sound_delay();
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
    use crate::settings::{MAX_SOUND_DELAY_MS, MIN_SOUND_DELAY_MS};
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
    fn delay_moves_only_the_part_that_must_come_later() {
        assert_eq!(delay_offsets(0), (0, 0));
        assert_eq!(delay_offsets(120), (0, 120_000_000));
        assert_eq!(delay_offsets(-120), (120_000_000, 0));
    }

    /// A 33-bit MPEG timestamp from its five bytes, in milliseconds.
    fn pes_time_ms(bytes: &[u8]) -> i64 {
        let high = i64::from(bytes[0] >> 1 & 0x07) << 30;
        let middle = i64::from(bytes[1]) << 22 | i64::from(bytes[2] >> 1) << 15;
        let low = i64::from(bytes[3]) << 7 | i64::from(bytes[4] >> 1);
        (high | middle | low) / 90
    }

    /// What a TV reads from the stream: the difference between sound and picture
    /// and the lowest margin of the sound before the stream's clock, both in
    /// milliseconds, from packets on a real UDP socket.
    fn read_stream_timing(delay_ms: i32, port: u16) -> (i64, i64) {
        const VIDEO_ID: u16 = 0x1011;
        const AUDIO_ID: u16 = 0x1100;
        gst::init().unwrap();
        let socket = std::net::UdpSocket::bind((LOCAL, port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let source = "videotestsrc is-live=true \
                      ! video/x-raw,format=BGRx,width=640,height=360 ! identity";
        let mut stream = ScreenStream::with_source(source.to_owned(), Encoder::software(), LOCAL);
        stream.sound_delay_ms.store(delay_ms, Ordering::Relaxed);
        let description = stream_description(
            &stream.source,
            stream.encoder,
            LOCAL,
            target(port, true),
            Some("audiotestsrc is-live=true wave=silence"),
        );
        let pipeline = gst::parse::launch(&description).unwrap();
        stream.pipeline = Some(pipeline.clone());
        stream.apply_sound_delay();
        pipeline.set_state(gst::State::Playing).unwrap();

        let (mut video, mut audio, mut clock) = (None, None, None);
        let mut lowest_margin = i64::MAX;
        let mut audio_frames = 0;
        let mut buffer = [0; 2048];
        // A second and a half of sound, at 1024 samples per AAC frame.
        while audio_frames < 70 {
            let length = socket.recv(&mut buffer).unwrap();
            for packet in buffer[12..length].chunks(188) {
                let id = u16::from_be_bytes([packet[1] & 0x1F, packet[2]]);
                let starts_unit = packet[1] & 0x40 != 0;
                let mut payload = 4;
                if packet[3] & 0x20 != 0 {
                    let adaptation = usize::from(packet[4]);
                    if adaptation >= 7 && packet[5] & 0x10 != 0 {
                        let raw = u64::from_be_bytes([
                            0, 0, 0, packet[6], packet[7], packet[8], packet[9], packet[10],
                        ]);
                        clock = Some(i64::try_from((raw >> 7) / 90).unwrap());
                    }
                    payload += 1 + adaptation;
                }
                if !starts_unit || packet[payload..payload + 3] != [0, 0, 1] {
                    continue;
                }
                let time = pes_time_ms(&packet[payload + 9..payload + 14]);
                if id == VIDEO_ID {
                    video.get_or_insert(time);
                } else if id == AUDIO_ID {
                    audio.get_or_insert(time);
                    audio_frames += 1;
                    if let Some(clock) = clock {
                        lowest_margin = lowest_margin.min(time - clock);
                    }
                }
            }
        }
        (audio.unwrap() - video.unwrap(), lowest_margin)
    }

    #[test]
    fn sound_delay_moves_the_times_in_the_stream_and_keeps_it_valid() {
        let tolerance = 60;
        let (baseline, _) = read_stream_timing(0, 29316);

        let (later, margin) = read_stream_timing(MAX_SOUND_DELAY_MS, 29317);
        assert!(
            (later - baseline - i64::from(MAX_SOUND_DELAY_MS)).abs() < tolerance,
            "baseline {baseline} ms, delayed {later} ms"
        );
        assert!(margin > 0, "sound after the clock by {margin} ms");

        let (earlier, margin) = read_stream_timing(MIN_SOUND_DELAY_MS, 29318);
        assert!(
            (earlier - baseline - i64::from(MIN_SOUND_DELAY_MS)).abs() < tolerance,
            "baseline {baseline} ms, picture delayed {earlier} ms"
        );
        assert!(margin > 0, "sound after the clock by {margin} ms");
    }

    /// Counts key frames in `frames` encoded frames, with the given distance.
    fn count_key_frames(encoder: Encoder, distance: u32, frames: u32) -> usize {
        gst::init().unwrap();
        let description = format!(
            "videotestsrc num-buffers={frames} pattern=ball \
             ! video/x-raw,format=NV12,width=640,height=360,framerate=30/1 \
             ! {} ! video/x-h264,profile=constrained-baseline \
             ! h264parse ! appsink name=out sync=false",
            encoder.description_with(Some(distance), None),
        );
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Bin>()
            .unwrap();
        let sink = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut key_frames = 0;
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(10)) {
            let buffer = sample.buffer().unwrap();
            if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                key_frames += 1;
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        key_frames
    }

    #[test]
    fn key_frame_distance_really_changes_the_stream() {
        let encoder = Encoder::software();
        assert!(count_key_frames(encoder, 30, 300) >= 9);
        assert!(count_key_frames(encoder, 120, 300) <= 4);
    }

    #[test]
    fn key_frame_distance_changes_the_detected_encoder_too() {
        gst::init().unwrap();
        let encoder = Encoder::detect().unwrap();
        assert!(count_key_frames(encoder, 30, 300) >= 9);
        assert!(count_key_frames(encoder, 120, 300) <= 4);
    }

    #[test]
    fn bitrate_override_is_a_valid_setting_for_every_encoder() {
        gst::init().unwrap();
        for encoder in [Encoder::software(), Encoder::detect().unwrap()] {
            let description = encoder.description_with(None, Some(3000));
            assert!(description.ends_with("bitrate=3000"), "{description}");
            gst::parse::launch(&description).unwrap();
        }
    }

    #[test]
    fn pacing_can_be_turned_off() {
        assert_eq!(pacing_options(0), "sync=false");
        assert_eq!(pacing_options(50), "sync=true ts-offset=50000000");
    }

    #[test]
    fn pacing_options_are_valid_for_the_sender() {
        gst::init().unwrap();
        gst::parse::launch(&format!(
            "fakesrc num-buffers=1 ! udpsink host=127.0.0.1 port=29320 {}",
            pacing_options(50)
        ))
        .unwrap();
    }

    #[test]
    fn stats_report_every_stage_of_a_real_stream() {
        gst::init().unwrap();
        let _receiver = std::net::UdpSocket::bind((LOCAL, 29319)).unwrap();
        let mut stream =
            ScreenStream::with_source(TEST_SOURCE.to_owned(), Encoder::software(), LOCAL);
        stream.stats_interval = Duration::from_secs(3);
        assert_eq!(stream.poll_stats(), None);
        stream.start(target(29319, false)).unwrap();

        let started = std::time::Instant::now();
        let stats = loop {
            if let Some(stats) = stream.poll_stats() {
                break stats;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "no stats arrived"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!((20..=31).contains(&stats.captured_fps), "{stats:?}");
        assert!(stats.to_encoder_fps > 0, "{stats:?}");
        assert!(stats.encoded_fps > 0, "{stats:?}");
        assert!(stats.sent_kbit_per_second > 0, "{stats:?}");
        assert!(stats.longest_capture_gap_ms > 0, "{stats:?}");
        assert_eq!(stream.poll_stats(), None, "must wait for the next period");
    }

    #[test]
    fn a_real_screen_stream_uses_the_system_sound() {
        let region = CaptureRegion {
            start_x: 0,
            start_y: 0,
            end_x: 1919,
            end_y: 1079,
        };
        let stream = ScreenStream::new(region, Encoder::software(), LOCAL, Arc::default());
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
