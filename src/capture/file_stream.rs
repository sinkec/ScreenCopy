//! Sends a video file to a TV, through the same path as a shared monitor.
//!
//! Two pipelines work side by side. The player decodes the file as far ahead as
//! its short queues allow, and the sender runs at a steady rate and never stops.
//! A thread moves between them, tick by tick, following one clock of its own: the
//! position in the film. While paused, or just after a seek, that position stands
//! still, so the sender repeats the last picture with silence, and the TV sees an
//! ordinary stream with nothing to react to. Picture and sound are both taken by
//! their time in the file, so they cannot drift apart.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gst::prelude::*;
use tracing::{debug, info, warn};

use super::recorder::Encoder;
use super::stream::{self, AAC_ENCODER, STATS_INTERVAL};
use super::stream_stats::{StreamMeter, StreamStats};
use crate::subtitles::Subtitles;
use crate::wfd::session::{MediaStream, Sound, StreamTarget};

const VIDEO_INPUT: &str = "file-video-in";
const AUDIO_INPUT: &str = "file-audio-in";
const SUBTITLE_OVERLAY: &str = "file-subtitles";
const VIDEO_OUTPUT: &str = "player-video-out";
const AUDIO_OUTPUT: &str = "player-audio-out";
const AUDIO_RATE: u64 = 48_000;
const AUDIO_CHANNELS: u64 = 2;
const AUDIO_BYTES_PER_SAMPLE: u64 = 2 * AUDIO_CHANNELS;
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;
const NANOSECONDS_PER_MILLISECOND: u64 = 1_000_000;
/// Roughly half a second of decoded pictures, and more than a second of sound.
const VIDEO_QUEUE_FRAMES: u32 = 16;
const AUDIO_QUEUE_BUFFERS: u32 = 64;
/// One stream of a file can run dry while the other has plenty. After this long
/// the film goes on rather than waiting forever, which also keeps the player's
/// queues from blocking each other.
const MAX_WAIT_FOR_DATA: Duration = Duration::from_secs(1);
/// A late thread catches up, unless it fell behind by more than this.
const MAX_CATCH_UP: Duration = Duration::from_secs(1);
const PROBE_TIMEOUT_SECONDS: u64 = 15;
const POSITION_REPORT_TICKS: u64 = 8;
const NO_SUBTITLE: &str = "";
/// What containers say when a track's language was not set.
const UNKNOWN_LANGUAGE: &str = "und";

/// How much data a film may use for its picture. More is sharper, up to what the
/// Wi-Fi connection can carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FilmQuality {
    Standard,
    #[default]
    High,
    Highest,
}

impl FilmQuality {
    fn bitrate_kbit(self, height: u32) -> u32 {
        let at_1080p = match self {
            Self::Standard => 8000,
            Self::High => 16_000,
            Self::Highest => 24_000,
        };
        // A picture's height is far below what this can overflow.
        at_1080p * height / 1080
    }
}

/// How large subtitles are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubtitleSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl SubtitleSize {
    /// A font for a picture of this height, roughly as tall as subtitles usually are.
    fn font(self, height: u32) -> String {
        let share = match self {
            Self::Small => 36,
            Self::Medium => 28,
            Self::Large => 22,
        };
        format!("Sans Bold {}", (height / share).max(8))
    }
}

/// Where subtitles are on the picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubtitlePosition {
    #[default]
    Bottom,
    /// Clear of a film's own captions and of a TV's overscan.
    Higher,
}

impl SubtitlePosition {
    fn shift(self, height: u32) -> i32 {
        match self {
            Self::Bottom => 0,
            // A picture's height is far below what this can overflow.
            Self::Higher => -i32::try_from(height / 10).unwrap_or(0),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct SubtitleOptions {
    subtitles: Option<Arc<Subtitles>>,
    /// Positive shows them later, negative earlier.
    delay_ms: i64,
    size: SubtitleSize,
    position: SubtitlePosition,
}

/// What the window can see about a running playback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackStatus {
    pub running: bool,
    pub paused: bool,
    pub ended: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug)]
struct Shared {
    status: PlaybackStatus,
    path: Option<PathBuf>,
    /// A different file to go on with, without leaving the session.
    load_requested: bool,
    seek_to_ms: Option<u64>,
    audio_track: Option<i32>,
    volume: f64,
    options: SubtitleOptions,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::default(),
            path: None,
            load_requested: false,
            seek_to_ms: None,
            audio_track: None,
            volume: 1.0,
            options: SubtitleOptions::default(),
        }
    }
}

/// What the window and the sending thread say to each other about the file that
/// is playing.
#[derive(Debug, Default)]
pub struct Playback {
    shared: Mutex<Shared>,
}

impl Playback {
    pub fn new(path: PathBuf) -> Self {
        let playback = Self::default();
        playback.load(path);
        playback
    }

    /// Goes on with this file from its start, also during a session.
    pub fn load(&self, path: PathBuf) {
        let mut shared = self.shared();
        shared.path = Some(path);
        shared.load_requested = true;
        shared.seek_to_ms = None;
        shared.audio_track = None;
        shared.status.position_ms = 0;
        shared.status.ended = false;
    }

    /// Goes on from the same place after the connection has been made again.
    pub fn prepare_to_go_on(&self) {
        let mut shared = self.shared();
        if shared.seek_to_ms.is_none() {
            shared.seek_to_ms = Some(shared.status.position_ms);
        }
    }

    pub fn set_volume(&self, volume: f64) {
        self.shared().volume = volume;
    }

    /// A panic elsewhere cannot make these plain values unusable.
    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn status(&self) -> PlaybackStatus {
        self.shared().status
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared().status.paused = paused;
    }

    pub fn seek(&self, position_ms: u64) {
        let mut shared = self.shared();
        shared.seek_to_ms = Some(position_ms);
        shared.status.position_ms = position_ms;
        shared.status.ended = false;
    }

    pub fn set_audio_track(&self, track: i32) {
        self.shared().audio_track = Some(track);
    }

    pub fn set_subtitles(&self, subtitles: Option<Arc<Subtitles>>) {
        self.shared().options.subtitles = subtitles;
    }

    pub fn set_subtitle_delay_ms(&self, delay_ms: i64) {
        self.shared().options.delay_ms = delay_ms;
    }

    pub fn set_subtitle_size(&self, size: SubtitleSize) {
        self.shared().options.size = size;
    }

    pub fn set_subtitle_position(&self, position: SubtitlePosition) {
        self.shared().options.position = position;
    }
}

/// What a file holds, read before anything is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub duration_ms: u64,
    /// One description for each sound track.
    pub audio_tracks: Vec<String>,
}

/// Opens a file to see whether it can be played. Blocks for a moment, so do not
/// call on the GTK thread.
pub fn read_file_info(path: &Path) -> Result<FileInfo, String> {
    let uri = gst::glib::filename_to_uri(path, None).map_err(|error| error.to_string())?;
    let make_fake = || {
        gst::ElementFactory::make("fakesink")
            .build()
            .map_err(|error| error.to_string())
    };
    let player = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("video-sink", make_fake()?)
        .property("audio-sink", make_fake()?)
        .build()
        .map_err(|error| error.to_string())?;
    player.set_property_from_str("flags", "video+audio");
    let result = read_info_from(&player);
    if let Err(error) = player.set_state(gst::State::Null) {
        debug!(%error, "cannot close the file");
    }
    result
}

fn read_info_from(player: &gst::Element) -> Result<FileInfo, String> {
    player
        .set_state(gst::State::Paused)
        .map_err(|_| "This file cannot be opened.".to_owned())?;
    let bus = player.bus().ok_or("This file cannot be opened.")?;
    let message = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(PROBE_TIMEOUT_SECONDS),
        &[gst::MessageType::AsyncDone, gst::MessageType::Error],
    );
    match message.as_ref().map(|message| message.view()) {
        Some(gst::MessageView::AsyncDone(_)) => {}
        Some(gst::MessageView::Error(error)) => {
            warn!(error = %error.error(), debug = ?error.debug(), "cannot read the file");
            return Err(
                "This file cannot be played. It may be damaged, or use a format \
                        that is not installed."
                    .to_owned(),
            );
        }
        _ => return Err("Opening this file took too long.".to_owned()),
    }
    if player.property::<i32>("n-video") == 0 && player.property::<i32>("n-audio") == 0 {
        return Err("This file has no picture or sound.".to_owned());
    }
    let tracks = player.property::<i32>("n-audio");
    Ok(FileInfo {
        duration_ms: player
            .query_duration::<gst::ClockTime>()
            .map_or(0, gst::ClockTime::mseconds),
        audio_tracks: (0..tracks)
            .map(|track| audio_track_name(player, track))
            .collect(),
    })
}

fn audio_track_name(player: &gst::Element, track: i32) -> String {
    let tags = player.emit_by_name::<Option<gst::TagList>>("get-audio-tags", &[&track]);
    let language = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::LanguageCode>())
        .map(|code| code.get().to_owned())
        .filter(|code| code != UNKNOWN_LANGUAGE);
    let title = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::Title>())
        .map(|title| title.get().to_owned());
    track_description(track, language.as_deref(), title.as_deref())
}

fn track_description(track: i32, language: Option<&str>, title: Option<&str>) -> String {
    match (language, title) {
        (Some(language), Some(title)) => format!("{language}: {title}"),
        (Some(text), None) | (None, Some(text)) => text.to_owned(),
        (None, None) => format!("Track {}", track + 1),
    }
}

/// Decoded sound, which can be read by its time in the file.
#[derive(Debug, Default)]
struct SoundQueue {
    /// Each part's start in the file, and its samples.
    parts: VecDeque<(u64, Vec<u8>)>,
}

impl SoundQueue {
    fn push(&mut self, start: u64, data: Vec<u8>) {
        self.parts.push_back((start, data));
    }

    fn clear(&mut self) {
        self.parts.clear();
    }

    fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Whether the queue reaches as far as `time`.
    fn reaches(&self, time: u64) -> bool {
        self.parts
            .back()
            .is_some_and(|(start, data)| start + bytes_to_duration(data.len()) >= time)
    }

    /// Fills `output` with the sound from `start` on, leaving silence where there
    /// is none, and forgets what lies before the end of it.
    fn read(&mut self, start: u64, output: &mut [u8]) {
        output.fill(0);
        let end = start + bytes_to_duration(output.len());
        for (part_start, data) in &self.parts {
            let part_end = part_start + bytes_to_duration(data.len());
            if *part_start >= end || part_end <= start {
                continue;
            }
            let from = duration_to_bytes(start.saturating_sub(*part_start));
            let to = duration_to_bytes(part_start.saturating_sub(start));
            let count = data
                .len()
                .saturating_sub(from)
                .min(output.len().saturating_sub(to));
            if let (Some(source), Some(target)) =
                (data.get(from..from + count), output.get_mut(to..to + count))
            {
                target.copy_from_slice(source);
            }
        }
        while self
            .parts
            .front()
            .is_some_and(|(part_start, data)| part_start + bytes_to_duration(data.len()) <= end)
        {
            self.parts.pop_front();
        }
    }
}

fn bytes_to_duration(bytes: usize) -> u64 {
    let samples = u64::try_from(bytes).unwrap_or(u64::MAX) / AUDIO_BYTES_PER_SAMPLE;
    samples.saturating_mul(NANOSECONDS_PER_SECOND) / AUDIO_RATE
}

/// Rounds to whole samples, so that left and right never swap.
fn duration_to_bytes(duration: u64) -> usize {
    let samples = (u128::from(duration) * u128::from(AUDIO_RATE)
        + u128::from(NANOSECONDS_PER_SECOND / 2))
        / u128::from(NANOSECONDS_PER_SECOND);
    usize::try_from(samples * u128::from(AUDIO_BYTES_PER_SAMPLE)).unwrap_or(usize::MAX)
}

fn video_output_description(width: u32, height: u32) -> String {
    format!(
        "videoconvert ! videoscale add-borders=true \
         ! video/x-raw,format=NV12,width={width},height={height},pixel-aspect-ratio=1/1 \
         ! appsink name={VIDEO_OUTPUT} sync=false qos=false max-buffers={VIDEO_QUEUE_FRAMES}"
    )
}

fn audio_output_description() -> String {
    format!(
        "audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,rate={AUDIO_RATE},channels={AUDIO_CHANNELS},layout=interleaved \
         ! appsink name={AUDIO_OUTPUT} sync=false qos=false max-buffers={AUDIO_QUEUE_BUFFERS}"
    )
}

/// Contains nothing from the file or the TV as text, so neither can alter the pipeline.
/// NVIDIA's encoder refuses a picture that does not say that it is progressive.
fn video_source_description(width: u32, height: u32, frame_rate: u32) -> String {
    format!(
        "appsrc name={VIDEO_INPUT} is-live=true format=time block=false \
         caps=video/x-raw,format=NV12,width={width},height={height},\
         framerate={frame_rate}/1,pixel-aspect-ratio=1/1,interlace-mode=progressive \
         ! textoverlay name={SUBTITLE_OVERLAY} auto-resize=false valignment=bottom \
         halignment=center line-alignment=center"
    )
}

fn audio_source_description() -> String {
    format!(
        "appsrc name={AUDIO_INPUT} is-live=true format=time block=false \
         caps=audio/x-raw,format=S16LE,rate={AUDIO_RATE},channels={AUDIO_CHANNELS},\
         layout=interleaved"
    )
}

fn element_of<T: IsA<gst::Element>>(bin: &gst::Bin, name: &str) -> Result<T, String> {
    bin.by_name(name)
        .and_then(|element| element.downcast::<T>().ok())
        .ok_or_else(|| format!("the pipeline lacks {name}"))
}

/// The player, and where its pictures and sound come out.
type Player = (gst::Element, gst_app::AppSink, Option<gst_app::AppSink>);

fn build_player(path: &Path, width: u32, height: u32, with_audio: bool) -> Result<Player, String> {
    let text = |error: gst::glib::Error| error.to_string();
    let uri = gst::glib::filename_to_uri(path, None).map_err(text)?;
    let video = gst::parse::bin_from_description(&video_output_description(width, height), true)
        .map_err(text)?;
    let video_output = element_of(video.upcast_ref(), VIDEO_OUTPUT)?;
    let player = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("video-sink", &video)
        .build()
        .map_err(|error| error.to_string())?;
    if !with_audio {
        player.set_property_from_str("flags", "video");
        return Ok((player, video_output, None));
    }
    let audio =
        gst::parse::bin_from_description(&audio_output_description(), true).map_err(text)?;
    let audio_output = element_of(audio.upcast_ref(), AUDIO_OUTPUT)?;
    player.set_property("audio-sink", &audio);
    // Soft volume is what makes the film's volume work without a sound device.
    player.set_property_from_str("flags", "video+audio+soft-volume");
    Ok((player, video_output, Some(audio_output)))
}

/// A decoded picture and its time in the file.
struct Frame {
    time: u64,
    buffer: gst::Buffer,
}

/// The time of a buffer in the file, which seeking does not change.
fn time_in_file(sample: &gst::Sample) -> Option<(u64, gst::Buffer)> {
    let buffer = sample.buffer_owned()?;
    let pts = buffer.pts()?;
    let segment = sample.segment()?.downcast_ref::<gst::ClockTime>()?;
    Some((segment.to_stream_time(pts)?.nseconds(), buffer))
}

/// A black picture for the moment before the first frame. Black in NV12 has a
/// luma of 16 and a chroma of 128.
fn black_frame(width: u32, height: u32) -> gst::Buffer {
    let width = usize::try_from(width).unwrap_or(0);
    let height = usize::try_from(height).unwrap_or(0);
    let luma = width * height;
    let mut data = vec![16; luma];
    data.resize(luma + luma / 2, 128);
    gst::Buffer::from_mut_slice(data)
}

/// The thread between the player and the sender.
struct Courier {
    playback: Arc<Playback>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    player: gst::Element,
    video_output: gst_app::AppSink,
    audio_output: Option<gst_app::AppSink>,
    video_input: gst_app::AppSrc,
    audio_input: Option<gst_app::AppSrc>,
    overlay: gst::Element,
    sender: gst::Element,
    height: u32,
    tick: u64,
    /// The position in the file, in nanoseconds.
    position: u64,
    frame: gst::Buffer,
    next_frame: Option<Frame>,
    /// The first picture after a seek is shown at once, also while paused.
    show_next_frame: bool,
    sound: SoundQueue,
    silence_samples: usize,
    has_video: bool,
    has_audio: bool,
    loaded: bool,
    waiting_since: Option<Instant>,
    shown_text: String,
    shown_font: String,
    shown_shift: i32,
    applied_audio_track: Option<i32>,
    applied_volume: f64,
}

impl Courier {
    fn run(mut self) {
        let frame_time = Duration::from_nanos(self.tick);
        let started = self
            .sender
            .current_running_time()
            .map_or(0, |time| time.nseconds());
        let mut sent_time = started;
        let mut deadline = Instant::now();
        let mut ticks = 0_u64;
        self.playback.shared().status.running = true;
        while !self.stop.load(Ordering::Relaxed) {
            self.follow_player();
            self.take_commands();
            self.take_decoded();
            let ended = self.has_ended();
            let paused = self.playback.status().paused;
            let advance = self.loaded && !paused && !ended && self.can_advance();
            if advance {
                self.position += self.tick;
            }
            self.show_current_frame();
            self.show_subtitle();
            if !self.send(sent_time, advance) {
                break;
            }
            sent_time += self.tick;
            ticks += 1;
            if ticks.is_multiple_of(POSITION_REPORT_TICKS) || ended {
                self.report(ended);
            }

            deadline += frame_time;
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            } else if now - deadline > MAX_CATCH_UP {
                // The timeline must stay continuous for the TV, whatever happened.
                debug!("the sending thread fell behind");
                deadline = now;
            }
        }
        self.playback.shared().status.running = false;
    }

    /// Learns what the file holds, and about the end of its decoding or errors in it.
    fn follow_player(&mut self) {
        let Some(bus) = self.player.bus() else {
            return;
        };
        while let Some(message) =
            bus.pop_filtered(&[gst::MessageType::AsyncDone, gst::MessageType::Error])
        {
            match message.view() {
                gst::MessageView::AsyncDone(_) => {
                    self.has_video = self.player.property::<i32>("n-video") > 0;
                    self.has_audio =
                        self.audio_output.is_some() && self.player.property::<i32>("n-audio") > 0;
                    self.loaded = true;
                }
                gst::MessageView::Error(error) => {
                    warn!(error = %error.error(), debug = ?error.debug(), "cannot play the file");
                    *self.failure.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(error.error().to_string());
                }
                _ => {}
            }
        }
    }

    fn take_commands(&mut self) {
        let (load, volume) = {
            let mut shared = self.playback.shared();
            let path = shared.path.clone().filter(|_| shared.load_requested);
            shared.load_requested = false;
            (path, shared.volume)
        };
        if let Some(path) = load {
            self.load(&path);
        }
        if (volume - self.applied_volume).abs() > f64::EPSILON {
            self.player.set_property("volume", volume);
            self.applied_volume = volume;
        }
        // A file that is still opening cannot be sought in or switched in yet.
        if !self.loaded {
            return;
        }
        let (seek, track) = {
            let mut shared = self.playback.shared();
            (shared.seek_to_ms.take(), shared.audio_track)
        };
        if let Some(position_ms) = seek {
            self.seek(position_ms);
        }
        if track != self.applied_audio_track {
            if let Some(track) = track {
                self.player.set_property("current-audio", track);
            }
            self.applied_audio_track = track;
        }
    }

    /// Changes the file behind a running stream, which simply goes on with the last
    /// picture and silence until the new one is ready.
    fn load(&mut self, path: &Path) {
        let uri = match gst::glib::filename_to_uri(path, None) {
            Ok(uri) => uri,
            Err(error) => {
                warn!(%error, "cannot use this file's name");
                return;
            }
        };
        if let Err(error) = self.player.set_state(gst::State::Ready) {
            warn!(%error, "cannot close the previous file");
        }
        self.player.set_property("uri", uri.as_str());
        if let Err(error) = self.player.set_state(gst::State::Playing) {
            warn!(%error, "cannot open the next file");
        }
        self.loaded = false;
        self.position = 0;
        self.next_frame = None;
        self.sound.clear();
        self.show_next_frame = true;
        self.waiting_since = None;
        self.applied_audio_track = None;
        info!("going on with the next file");
    }

    fn seek(&mut self, position_ms: u64) {
        let target = gst::ClockTime::from_mseconds(position_ms);
        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
        if let Err(error) = self.player.seek_simple(flags, target) {
            warn!(%error, "cannot seek in this file");
            return;
        }
        self.position = target.nseconds();
        self.next_frame = None;
        self.sound.clear();
        self.show_next_frame = true;
        self.waiting_since = None;
        info!(position_ms, "seeking");
    }

    /// Takes what the player has ready, without ever waiting for it.
    fn take_decoded(&mut self) {
        if self.next_frame.is_none()
            && let Some(sample) = self.video_output.try_pull_sample(gst::ClockTime::ZERO)
            && let Some((time, buffer)) = time_in_file(&sample)
        {
            self.next_frame = Some(Frame { time, buffer });
        }
        let Some(output) = &self.audio_output else {
            return;
        };
        let wanted = self.position + 2 * self.tick;
        while !self.sound.reaches(wanted) {
            let Some(sample) = output.try_pull_sample(gst::ClockTime::ZERO) else {
                return;
            };
            let Some((time, buffer)) = time_in_file(&sample) else {
                continue;
            };
            match buffer.map_readable() {
                Ok(map) => self.sound.push(time, map.as_slice().to_vec()),
                Err(error) => debug!(%error, "cannot read decoded sound"),
            }
        }
    }

    fn video_finished(&self) -> bool {
        !self.has_video || (self.next_frame.is_none() && self.video_output.is_eos())
    }

    fn audio_finished(&self) -> bool {
        !self.has_audio
            || self
                .audio_output
                .as_ref()
                .is_none_or(|output| self.sound.is_empty() && output.is_eos())
    }

    fn has_ended(&self) -> bool {
        self.loaded && self.video_finished() && self.audio_finished()
    }

    /// Whether the data for the next moment is here. Waiting too long counts too.
    fn can_advance(&mut self) -> bool {
        let next = self.position + self.tick;
        let video_ready = self.next_frame.is_some() || self.video_finished();
        let audio_ready = self.sound.reaches(next) || self.audio_finished();
        if video_ready && audio_ready {
            self.waiting_since = None;
            return true;
        }
        let since = *self.waiting_since.get_or_insert_with(Instant::now);
        if since.elapsed() > MAX_WAIT_FOR_DATA {
            debug!(
                video_ready,
                audio_ready, "going on without waiting for data"
            );
            return true;
        }
        false
    }

    fn show_current_frame(&mut self) {
        loop {
            let due = self
                .next_frame
                .as_ref()
                .is_some_and(|frame| frame.time <= self.position || self.show_next_frame);
            if !due {
                return;
            }
            if let Some(frame) = self.next_frame.take() {
                self.frame = frame.buffer;
                self.show_next_frame = false;
            }
            if let Some(sample) = self.video_output.try_pull_sample(gst::ClockTime::ZERO)
                && let Some((time, buffer)) = time_in_file(&sample)
            {
                self.next_frame = Some(Frame { time, buffer });
            }
        }
    }

    fn show_subtitle(&mut self) {
        let options = self.playback.shared().options.clone();
        let position_ms = self.position / NANOSECONDS_PER_MILLISECOND;
        let time = i64::try_from(position_ms)
            .unwrap_or(i64::MAX)
            .saturating_sub(options.delay_ms);
        let text = u64::try_from(time)
            .ok()
            .and_then(|time| options.subtitles.as_deref()?.text_at(time))
            .unwrap_or(NO_SUBTITLE);
        if text != self.shown_text {
            self.overlay.set_property("text", overlay_text(text));
            text.clone_into(&mut self.shown_text);
        }
        let font = options.size.font(self.height);
        if font != self.shown_font {
            self.overlay.set_property("font-desc", &font);
            self.shown_font = font;
        }
        let shift = options.position.shift(self.height);
        if shift != self.shown_shift {
            self.overlay.set_property("deltay", shift);
            self.shown_shift = shift;
        }
    }

    /// One tick's picture and sound, the latter only if the position has moved on.
    /// `false` means that the sender has gone away.
    fn send(&mut self, sent_time: u64, advanced: bool) -> bool {
        let time = gst::ClockTime::from_nseconds(sent_time);
        let duration = gst::ClockTime::from_nseconds(self.tick);
        let mut frame = self.frame.copy();
        {
            let frame = frame.make_mut();
            frame.set_pts(time);
            frame.set_duration(duration);
        }
        if self.video_input.push_buffer(frame).is_err() {
            return false;
        }
        let Some(audio_input) = &self.audio_input else {
            return true;
        };
        let mut data =
            vec![0; self.silence_samples * usize::try_from(AUDIO_BYTES_PER_SAMPLE).unwrap_or(4)];
        if advanced {
            self.sound.read(self.position - self.tick, &mut data);
        }
        let mut sound = gst::Buffer::from_mut_slice(data);
        {
            let sound = sound.make_mut();
            sound.set_pts(time);
            sound.set_duration(duration);
        }
        audio_input.push_buffer(sound).is_ok()
    }

    fn report(&mut self, ended: bool) {
        let duration = self.player.query_duration::<gst::ClockTime>();
        let mut shared = self.playback.shared();
        if shared.seek_to_ms.is_none() {
            shared.status.position_ms = self.position / NANOSECONDS_PER_MILLISECOND;
        }
        shared.status.ended = ended;
        if let Some(duration) = duration {
            shared.status.duration_ms = duration.mseconds();
        }
    }
}

/// Subtitles are plain text, but the overlay reads its text as markup.
fn overlay_text(text: &str) -> String {
    gst::glib::markup_escape_text(text).to_string()
}

pub struct FileStream {
    playback: Arc<Playback>,
    encoder: Encoder,
    quality: FilmQuality,
    local_address: Ipv4Addr,
    sound_delay_ms: Arc<AtomicI32>,
    applied_sound_delay_ms: Option<i32>,
    sender: Option<gst::Element>,
    player: Option<gst::Element>,
    courier: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    meter: Option<StreamMeter>,
}

impl FileStream {
    pub fn new(
        playback: Arc<Playback>,
        encoder: Encoder,
        quality: FilmQuality,
        local_address: Ipv4Addr,
        sound_delay_ms: Arc<AtomicI32>,
    ) -> Self {
        Self {
            playback,
            encoder,
            quality,
            local_address,
            sound_delay_ms,
            applied_sound_delay_ms: None,
            sender: None,
            player: None,
            courier: None,
            stop: Arc::default(),
            failure: Arc::default(),
            meter: None,
        }
    }
}

impl MediaStream for FileStream {
    fn start(&mut self, target: StreamTarget) -> Result<Sound, String> {
        let mode = target.mode;
        let with_audio = target.with_audio && gst::ElementFactory::find(AAC_ENCODER).is_some();
        let source = video_source_description(mode.width, mode.height, mode.frame_rate);
        let audio = with_audio.then(audio_source_description);
        let bitrate = std::env::var(stream::BITRATE_VARIABLE_NAME)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| self.quality.bitrate_kbit(mode.height));
        let encoder = self.encoder.film_description(bitrate, mode.frame_rate);
        let description = stream::stream_description_with(
            &encoder,
            &source,
            self.local_address,
            target,
            audio.as_deref(),
        );
        debug!(%description, "building the file's stream pipeline");
        let sender = gst::parse::launch(&description).map_err(|error| error.to_string())?;
        let bin = sender
            .downcast_ref::<gst::Bin>()
            .ok_or("the stream pipeline has an unexpected structure")?;
        let video_input = element_of(bin, VIDEO_INPUT)?;
        let audio_input = with_audio
            .then(|| element_of(bin, AUDIO_INPUT))
            .transpose()?;
        let overlay = element_of(bin, SUBTITLE_OVERLAY)?;
        self.meter = Some(stream::attach_meter(bin, STATS_INTERVAL));

        let path = {
            let mut shared = self.playback.shared();
            shared.load_requested = false;
            shared.path.clone().ok_or("no file was chosen")?
        };
        let (player, video_output, audio_output) =
            build_player(&path, mode.width, mode.height, with_audio)?;
        sender
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        player
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        self.sender = Some(sender.clone());
        self.player = Some(player.clone());
        self.apply_sound_delay();

        let tick = NANOSECONDS_PER_SECOND / u64::from(mode.frame_rate);
        {
            // A seek that is waiting says where to begin.
            let mut shared = self.playback.shared();
            shared.status = PlaybackStatus {
                position_ms: shared.seek_to_ms.unwrap_or(0),
                ..PlaybackStatus::default()
            };
        }
        let courier = Courier {
            playback: Arc::clone(&self.playback),
            stop: Arc::clone(&self.stop),
            failure: Arc::clone(&self.failure),
            player,
            video_output,
            audio_output,
            video_input,
            audio_input,
            overlay,
            sender,
            height: mode.height,
            tick,
            position: 0,
            frame: black_frame(mode.width, mode.height),
            next_frame: None,
            show_next_frame: true,
            sound: SoundQueue::default(),
            silence_samples: usize::try_from(AUDIO_RATE / u64::from(mode.frame_rate)).unwrap_or(0),
            has_video: true,
            has_audio: with_audio,
            loaded: false,
            waiting_since: None,
            shown_text: NO_SUBTITLE.to_owned(),
            shown_font: String::new(),
            shown_shift: 0,
            applied_audio_track: None,
            applied_volume: 1.0,
        };
        let handle = std::thread::Builder::new()
            .name("file-courier".to_owned())
            .spawn(move || courier.run())
            .map_err(|error| error.to_string())?;
        self.courier = Some(handle);
        Ok(if with_audio {
            Sound::System
        } else {
            Sound::Absent
        })
    }

    fn poll_stats(&mut self) -> Option<StreamStats> {
        self.meter.as_mut()?.poll()
    }

    fn poll_failure(&mut self) -> Option<String> {
        self.apply_sound_delay();
        if let Some(failure) = self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Some(failure);
        }
        stream::pipeline_failure(self.sender.as_ref()?)
    }
}

impl FileStream {
    fn apply_sound_delay(&mut self) {
        let delay = self.sound_delay_ms.load(Ordering::Relaxed);
        if self.applied_sound_delay_ms == Some(delay) {
            return;
        }
        if let Some(sender) = &self.sender {
            stream::apply_delay(sender, delay);
            self.applied_sound_delay_ms = Some(delay);
        }
    }
}

impl Drop for FileStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(courier) = self.courier.take()
            && courier.join().is_err()
        {
            warn!("the sending thread failed");
        }
        for pipeline in [&self.player, &self.sender].into_iter().flatten() {
            if let Err(error) = pipeline.set_state(gst::State::Null) {
                warn!(%error, "cannot stop the file's pipeline");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::UdpSocket;

    use super::*;
    use crate::wfd::params::choose_video_mode;

    const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;
    const FORMATS_720P30: &str = "00 00 01 01 00000020 00000000 00000000 00 0000 0000 00 none none";

    /// A short real file with moving picture and sound, written once per test.
    fn sample_file(name: &str, seconds: u32, with_audio: bool) -> PathBuf {
        gst::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("monitorbeam-{name}-{}.mkv", std::process::id()));
        let frames = seconds * 30;
        let audio = if with_audio {
            format!(
                "audiotestsrc num-buffers={} samplesperbuffer=1024 \
                 ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! avenc_aac ! mux.",
                seconds * 48_000 / 1024
            )
        } else {
            String::new()
        };
        let description = format!(
            "videotestsrc num-buffers={frames} pattern=ball \
             ! video/x-raw,format=I420,width=640,height=360,framerate=30/1 \
             ! x264enc speed-preset=ultrafast ! h264parse ! mux. {audio} \
             matroskamux name=mux ! filesink location={}",
            path.display()
        );
        let pipeline = gst::parse::launch(&description).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(60),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
        pipeline.set_state(gst::State::Null).unwrap();
        path
    }

    fn target(port: u16, with_audio: bool) -> StreamTarget {
        StreamTarget {
            address: LOCAL,
            port,
            local_port: 0,
            mode: choose_video_mode(FORMATS_720P30).unwrap(),
            with_audio,
        }
    }

    /// Counts the packets that keep arriving, as a TV would receive them.
    fn received_during(socket: &UdpSocket, period: Duration) -> usize {
        let mut buffer = [0; 2048];
        let started = Instant::now();
        let mut packets = 0;
        while started.elapsed() < period {
            if socket.recv(&mut buffer).is_ok() {
                packets += 1;
            }
        }
        packets
    }

    fn receiver(port: u16) -> UdpSocket {
        let socket = UdpSocket::bind((LOCAL, port)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        socket
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < Duration::from_secs(20), "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_file_keeps_streaming_while_paused_and_goes_on_from_the_same_place() {
        let path = sample_file("pause", 8, true);
        let socket = receiver(29340);
        let playback = Arc::new(Playback::new(path.clone()));
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::software(),
            FilmQuality::Standard,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29340, true)).unwrap();
        wait_until("the film did not start", || {
            playback.status().position_ms > 500
        });
        assert_eq!(stream.poll_failure(), None);

        playback.set_paused(true);
        std::thread::sleep(Duration::from_millis(300));
        let paused_at = playback.status().position_ms;
        let packets = received_during(&socket, Duration::from_millis(1200));
        assert!(packets > 0, "the stream stopped during the pause");
        assert_eq!(
            playback.status().position_ms,
            paused_at,
            "the film moved on"
        );

        playback.set_paused(false);
        wait_until("the film did not go on", || {
            playback.status().position_ms > paused_at + 300
        });
        assert_eq!(stream.poll_failure(), None);
        drop(stream);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn seeking_moves_the_film_and_the_stream_goes_on() {
        let path = sample_file("seek", 8, true);
        let socket = receiver(29341);
        let playback = Arc::new(Playback::new(path.clone()));
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::software(),
            FilmQuality::Standard,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29341, true)).unwrap();
        wait_until("the film did not start", || {
            playback.status().position_ms > 300
        });

        playback.seek(5000);
        wait_until("the seek did not happen", || {
            playback.status().position_ms >= 5000
        });
        assert!(received_during(&socket, Duration::from_millis(500)) > 0);
        wait_until("the film did not go on after the seek", || {
            playback.status().position_ms > 5300
        });

        playback.seek(1000);
        wait_until("the backward seek did not happen", || {
            let position = playback.status().position_ms;
            (1000..4000).contains(&position)
        });
        assert_eq!(stream.poll_failure(), None);
        drop(stream);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_end_of_a_file_keeps_the_stream_and_can_be_left_by_seeking() {
        let path = sample_file("end", 2, true);
        let socket = receiver(29342);
        let playback = Arc::new(Playback::new(path.clone()));
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::software(),
            FilmQuality::Standard,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29342, true)).unwrap();
        wait_until("the end was not noticed", || playback.status().ended);
        assert!(received_during(&socket, Duration::from_millis(500)) > 0);
        assert_eq!(stream.poll_failure(), None);

        playback.seek(0);
        wait_until("the film did not start again", || {
            let status = playback.status();
            !status.ended && (1..1500).contains(&status.position_ms)
        });
        drop(stream);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_file_reaches_the_receiver_through_the_detected_encoder() {
        let path = sample_file("detected", 3, true);
        let socket = receiver(29346);
        let playback = Arc::new(Playback::new(path.clone()));
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::detect().unwrap(),
            FilmQuality::High,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29346, true)).unwrap();
        wait_until("the film did not start", || {
            playback.status().position_ms > 500
        });
        assert!(received_during(&socket, Duration::from_millis(300)) > 0);
        assert_eq!(stream.poll_failure(), None);
        drop(stream);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_session_can_begin_at_a_position() {
        let path = sample_file("resume", 6, true);
        let _socket = receiver(29347);
        let playback = Arc::new(Playback::new(path.clone()));
        playback.seek(3000);
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::software(),
            FilmQuality::Standard,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29347, true)).unwrap();
        assert!(playback.status().position_ms >= 3000);
        wait_until("the film did not go on from there", || {
            playback.status().position_ms > 3300
        });
        assert!(playback.status().position_ms < 5500);
        drop(stream);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn the_next_file_plays_in_the_same_session() {
        let first = sample_file("first", 2, true);
        let second = sample_file("second", 5, true);
        let socket = receiver(29348);
        let playback = Arc::new(Playback::new(first.clone()));
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::software(),
            FilmQuality::Standard,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29348, true)).unwrap();
        wait_until("the first file did not end", || playback.status().ended);

        playback.load(second.clone());
        wait_until("the second file did not start", || {
            let status = playback.status();
            !status.ended && status.position_ms > 300 && status.duration_ms > 4000
        });
        assert!(received_during(&socket, Duration::from_millis(300)) > 0);
        assert_eq!(stream.poll_failure(), None);
        drop(stream);
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }

    #[test]
    fn the_film_goes_on_from_the_same_place_after_a_new_connection() {
        let path = sample_file("reconnect", 6, true);
        let playback = Arc::new(Playback::new(path.clone()));
        for port in [29349, 29350] {
            let _socket = receiver(port);
            let mut stream = FileStream::new(
                Arc::clone(&playback),
                Encoder::software(),
                FilmQuality::Standard,
                LOCAL,
                Arc::default(),
            );
            let before = playback.status().position_ms;
            stream.start(target(port, true)).unwrap();
            wait_until("the film did not go on", || {
                playback.status().position_ms > before + 800
            });
            drop(stream);
            playback.prepare_to_go_on();
        }
        assert!(playback.status().position_ms > 1600);
        std::fs::remove_file(path).unwrap();
    }

    /// The loudest sample that comes out of the player at this volume.
    fn loudest_sample(volume: f64) -> i16 {
        let path = sample_file(&format!("volume-{}", (volume * 10.0) as u32), 2, true);
        let (player, video, audio) = build_player(&path, 320, 180, true).unwrap();
        let audio = audio.unwrap();
        player.set_property("volume", volume);
        player.set_state(gst::State::Playing).unwrap();
        let mut loudest = 0;
        for _ in 0..50 {
            while video.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
            // The player's queues hold a picture and sound back from each other.
            let Some(sample) = audio.try_pull_sample(gst::ClockTime::from_mseconds(200)) else {
                continue;
            };
            let buffer = sample.buffer().unwrap();
            let map = buffer.map_readable().unwrap();
            for value in map.as_slice().as_chunks::<2>().0 {
                loudest = loudest.max(i16::from_le_bytes(*value));
            }
        }
        player.set_state(gst::State::Null).unwrap();
        std::fs::remove_file(path).unwrap();
        loudest
    }

    #[test]
    fn the_volume_really_changes_the_sound() {
        let normal = loudest_sample(1.0);
        assert!(normal > 1000, "{normal}");
        assert!(i32::from(loudest_sample(0.5)) < i32::from(normal) * 3 / 4);
        assert_eq!(loudest_sample(0.0), 0);
    }

    #[test]
    fn film_settings_work_with_every_encoder() {
        gst::init().unwrap();
        for encoder in [Encoder::software(), Encoder::detect().unwrap()] {
            let description = format!(
                "videotestsrc num-buffers=30 ! video/x-raw,format=NV12,width=640,height=360 \
                 ! {} ! video/x-h264,profile=constrained-baseline ! fakesink",
                encoder.film_description(16_000, 30)
            );
            let pipeline = gst::parse::launch(&description).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            let message = pipeline
                .bus()
                .unwrap()
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(20),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .unwrap();
            assert_eq!(message.type_(), gst::MessageType::Eos, "{encoder:?}");
            pipeline.set_state(gst::State::Null).unwrap();
        }
    }

    #[test]
    fn film_quality_follows_the_picture_size() {
        assert_eq!(FilmQuality::High.bitrate_kbit(1080), 16_000);
        assert!(FilmQuality::High.bitrate_kbit(720) < FilmQuality::High.bitrate_kbit(1080));
        assert!(FilmQuality::Highest.bitrate_kbit(1080) > FilmQuality::High.bitrate_kbit(1080));
    }

    #[test]
    fn a_file_without_sound_plays_with_silence() {
        let path = sample_file("silent", 3, false);
        let _socket = receiver(29343);
        let playback = Arc::new(Playback::new(path.clone()));
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::software(),
            FilmQuality::Standard,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29343, true)).unwrap();
        wait_until("the film did not start", || {
            playback.status().position_ms > 500
        });
        assert_eq!(stream.poll_failure(), None);
        drop(stream);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stopping_ends_the_sending_thread() {
        let path = sample_file("stop", 3, true);
        let _socket = receiver(29344);
        let playback = Arc::new(Playback::new(path.clone()));
        let mut stream = FileStream::new(
            Arc::clone(&playback),
            Encoder::software(),
            FilmQuality::Standard,
            LOCAL,
            Arc::default(),
        );
        stream.start(target(29344, true)).unwrap();
        wait_until("the thread did not start", || playback.status().running);
        drop(stream);
        assert!(!playback.status().running);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_what_a_file_holds() {
        let path = sample_file("info", 2, true);
        let info = read_file_info(&path).unwrap();
        assert!((1500..2500).contains(&info.duration_ms), "{info:?}");
        assert_eq!(info.audio_tracks.len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_file_that_is_no_video_is_refused_without_a_panic() {
        gst::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("monitorbeam-not-video-{}.mkv", std::process::id()));
        std::fs::write(&path, [0_u8; 4096]).unwrap();
        assert!(read_file_info(&path).is_err());
        std::fs::remove_file(path).unwrap();
        assert!(read_file_info(Path::new("/nonexistent/film.mkv")).is_err());
    }

    #[test]
    fn sound_is_read_by_its_time_in_the_file() {
        // 1000 samples of 1 from the start, then a gap, then 1000 of 2.
        let part = |value: u8, samples: usize| vec![value; samples * 4];
        let time = |samples: u64| samples * NANOSECONDS_PER_SECOND / AUDIO_RATE;
        let mut queue = SoundQueue::default();
        queue.push(0, part(1, 1000));
        queue.push(time(2000), part(2, 1000));
        assert!(queue.reaches(time(2999)));
        assert!(!queue.reaches(time(3001)));

        let mut output = part(9, 500);
        queue.read(time(750), &mut output);
        assert_eq!(&output[..250 * 4], &part(1, 250)[..]);
        assert_eq!(&output[250 * 4..], &part(0, 250)[..]);

        let mut output = part(9, 500);
        queue.read(time(1800), &mut output);
        assert_eq!(&output[..200 * 4], &part(0, 200)[..]);
        assert_eq!(&output[200 * 4..], &part(2, 300)[..]);
    }

    #[test]
    fn sound_that_was_read_is_forgotten() {
        let time = |samples: u64| samples * NANOSECONDS_PER_SECOND / AUDIO_RATE;
        let mut queue = SoundQueue::default();
        queue.push(0, vec![1; 400]);
        let mut output = [0; 400];
        queue.read(time(0), &mut output);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_black_frame_has_the_size_of_the_picture() {
        gst::init().unwrap();
        assert_eq!(black_frame(1280, 720).size(), 1280 * 720 * 3 / 2);
    }

    #[test]
    fn tracks_are_described_by_what_is_known_about_them() {
        assert_eq!(track_description(0, None, None), "Track 1");
        assert_eq!(track_description(1, Some("sl"), None), "sl");
        assert_eq!(track_description(1, None, Some("Director")), "Director");
        assert_eq!(
            track_description(1, Some("en"), Some("Stereo")),
            "en: Stereo"
        );
    }

    #[test]
    fn subtitle_text_cannot_act_as_markup() {
        gst::init().unwrap();
        assert_eq!(overlay_text("a < b & c"), "a &lt; b &amp; c");
    }

    #[test]
    fn subtitle_sizes_follow_the_picture() {
        assert_eq!(SubtitleSize::Medium.font(1080), "Sans Bold 38");
        assert_eq!(SubtitleSize::Medium.font(720), "Sans Bold 25");
        assert_eq!(SubtitlePosition::Bottom.shift(1080), 0);
        assert_eq!(SubtitlePosition::Higher.shift(1080), -108);
    }

    /// A file that flashes white and beeps at the same moment, once per second.
    fn flash_and_beep_file(name: &str, seconds: u64) -> PathBuf {
        const WIDTH: usize = 320;
        const HEIGHT: usize = 180;
        const FLASH_FRAMES: u64 = 3;
        const BEEP_SAMPLES: u64 = 4800;
        const AUDIO_CHUNK: u64 = 1600;
        gst::init().unwrap();
        let path =
            std::env::temp_dir().join(format!("monitorbeam-{name}-{}.mkv", std::process::id()));
        let description = format!(
            "appsrc name=v format=time is-live=false \
             caps=video/x-raw,format=GRAY8,width={WIDTH},height={HEIGHT},framerate=30/1 \
             ! videoconvert ! video/x-raw,format=I420 \
             ! x264enc speed-preset=ultrafast tune=zerolatency ! h264parse ! mux. \
             appsrc name=a format=time is-live=false \
             caps=audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! audioconvert ! avenc_aac bitrate=128000 ! aacparse ! mux. \
             matroskamux name=mux ! filesink location={}",
            path.display()
        );
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Bin>()
            .unwrap();
        let video = element_of::<gst_app::AppSrc>(&pipeline, "v").unwrap();
        let audio = element_of::<gst_app::AppSrc>(&pipeline, "a").unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        for frame in 0..seconds * 30 {
            let value = if frame % 30 < FLASH_FRAMES { 235 } else { 16 };
            let mut buffer = gst::Buffer::from_mut_slice(vec![value; WIDTH * HEIGHT]);
            buffer.make_mut().set_pts(gst::ClockTime::from_nseconds(
                frame * NANOSECONDS_PER_SECOND / 30,
            ));
            video.push_buffer(buffer).unwrap();
            let chunk_start = frame * AUDIO_CHUNK;
            let mut samples = Vec::with_capacity(AUDIO_CHUNK as usize * 4);
            for sample in chunk_start..chunk_start + AUDIO_CHUNK {
                let beeping = sample % AUDIO_RATE < BEEP_SAMPLES;
                let square = if sample / 8 % 2 == 0 {
                    20_000_i16
                } else {
                    -20_000
                };
                let value = if beeping { square } else { 0 };
                samples.extend_from_slice(&value.to_le_bytes());
                samples.extend_from_slice(&value.to_le_bytes());
            }
            let mut buffer = gst::Buffer::from_mut_slice(samples);
            buffer.make_mut().set_pts(gst::ClockTime::from_nseconds(
                chunk_start * NANOSECONDS_PER_SECOND / AUDIO_RATE,
            ));
            audio.push_buffer(buffer).unwrap();
        }
        video.end_of_stream().unwrap();
        audio.end_of_stream().unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(60),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
        pipeline.set_state(gst::State::Null).unwrap();
        path
    }

    /// When each flash and each beep starts in the received stream, in milliseconds.
    struct Onsets {
        flashes: Vec<i64>,
        beeps: Vec<i64>,
    }

    fn pull_onsets(
        pipeline: &gst::Bin,
        name: &str,
        detect: fn(&[u8]) -> Option<usize>,
        bytes_per_ms: usize,
    ) -> Vec<i64> {
        let sink = element_of::<gst_app::AppSink>(pipeline, name).unwrap();
        let mut onsets = Vec::new();
        let mut was_on = false;
        let mut last_on = 0;
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(2)) {
            let buffer = sample.buffer().unwrap();
            let time = i64::try_from(buffer.pts().unwrap().mseconds()).unwrap();
            let map = buffer.map_readable().unwrap();
            match detect(map.as_slice()) {
                Some(offset) => {
                    let at = time + i64::try_from(offset / bytes_per_ms.max(1)).unwrap();
                    if !was_on && at - last_on > 300 {
                        onsets.push(at);
                    }
                    was_on = true;
                    last_on = at;
                }
                None => was_on = false,
            }
        }
        onsets
    }

    fn bright(frame: &[u8]) -> Option<usize> {
        (frame.first().is_some_and(|luma| *luma > 128)).then_some(0)
    }

    fn loud(samples: &[u8]) -> Option<usize> {
        samples
            .as_chunks::<2>()
            .0
            .iter()
            .position(|sample| i16::from_le_bytes(*sample).unsigned_abs() > 5000)
            .map(|position| position * 2)
    }

    /// The real path to a TV, and the real path from one back to pictures and sound.
    fn receive_onsets(port: u16, run: impl FnOnce()) -> Onsets {
        let description = format!(
            "udpsrc address=127.0.0.1 port={port} buffer-size=4194304 \
             caps=application/x-rtp,media=video,clock-rate=90000,encoding-name=MP2T \
             ! rtpmp2tdepay ! tsdemux name=demux \
             demux. ! queue max-size-time=0 max-size-bytes=0 max-size-buffers=0 ! h264parse \
             ! avdec_h264 ! videoconvert ! video/x-raw,format=GRAY8 \
             ! appsink name=seen sync=false max-buffers=0 \
             demux. ! queue max-size-time=0 max-size-bytes=0 max-size-buffers=0 ! aacparse \
             ! avdec_aac ! audioconvert ! audio/x-raw,format=S16LE,channels=1 \
             ! appsink name=heard sync=false max-buffers=0"
        );
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Bin>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        run();
        // Bytes per millisecond of one 48 kHz channel of 16-bit samples.
        let beeps = pull_onsets(&pipeline, "heard", loud, 96);
        let flashes = pull_onsets(&pipeline, "seen", bright, 1);
        pipeline.set_state(gst::State::Null).unwrap();
        Onsets { flashes, beeps }
    }

    /// The largest difference between a flash and the beep nearest to it, and the
    /// number of flashes.
    fn largest_difference(onsets: &Onsets) -> i64 {
        onsets
            .flashes
            .iter()
            .map(|flash| {
                onsets
                    .beeps
                    .iter()
                    .map(|beep| (beep - flash).abs())
                    .min()
                    .unwrap_or(i64::MAX)
            })
            .max()
            .unwrap_or(i64::MAX)
    }

    #[test]
    fn picture_and_sound_stay_together_through_pause_and_seek() {
        const PORT: u16 = 29345;
        let path = flash_and_beep_file("sync", 14);
        let onsets = receive_onsets(PORT, || {
            let playback = Arc::new(Playback::new(path.clone()));
            let mut stream = FileStream::new(
                Arc::clone(&playback),
                Encoder::software(),
                FilmQuality::Standard,
                LOCAL,
                Arc::default(),
            );
            stream.start(target(PORT, true)).unwrap();
            wait_until("the film did not start", || {
                playback.status().position_ms > 2500
            });
            playback.set_paused(true);
            std::thread::sleep(Duration::from_millis(1200));
            playback.set_paused(false);
            let resumed = playback.status().position_ms;
            wait_until("the film did not go on", || {
                playback.status().position_ms > resumed + 2200
            });
            playback.seek(9000);
            wait_until("the film did not go on after the seek", || {
                playback.status().position_ms > 12_200
            });
            assert_eq!(stream.poll_failure(), None);
        });
        std::fs::remove_file(&path).unwrap();
        eprintln!("flashes {:?}", onsets.flashes);
        eprintln!("beeps   {:?}", onsets.beeps);
        assert!(onsets.flashes.len() >= 6, "{:?}", onsets.flashes);
        let difference = largest_difference(&onsets);
        assert!(difference <= 70, "{difference} ms apart");
    }

    #[test]
    fn pipelines_are_valid() {
        gst::init().unwrap();
        gst::parse::launch(&format!(
            "{} ! fakesink",
            video_source_description(1280, 720, 30)
        ))
        .unwrap();
        gst::parse::launch(&format!("{} ! fakesink", audio_source_description())).unwrap();
        gst::parse::bin_from_description(&video_output_description(1280, 720), true).unwrap();
        gst::parse::bin_from_description(&audio_output_description(), true).unwrap();
    }
}
