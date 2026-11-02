//! Measures how frames move through the stream pipeline, to show where a stall
//! starts. It only counts buffers and their timing, never looks at their contents.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gst::prelude::*;

/// One point in the pipeline that buffers pass.
#[derive(Debug, Default)]
struct Stage {
    buffers: AtomicU64,
    bytes: AtomicU64,
    /// Nanoseconds since the measurement began; zero before the first buffer.
    last_seen: AtomicU64,
    longest_gap: AtomicU64,
}

impl Stage {
    fn record(&self, now: u64, bytes: u64) {
        self.buffers.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let previous = self.last_seen.swap(now, Ordering::Relaxed);
        if previous != 0 {
            self.longest_gap
                .fetch_max(now.saturating_sub(previous), Ordering::Relaxed);
        }
    }

    /// The counts since the last call.
    fn take(&self) -> StageSnapshot {
        StageSnapshot {
            buffers: self.buffers.swap(0, Ordering::Relaxed),
            bytes: self.bytes.swap(0, Ordering::Relaxed),
            longest_gap: self.longest_gap.swap(0, Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StageSnapshot {
    buffers: u64,
    bytes: u64,
    longest_gap: u64,
}

/// What happened during one measuring period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStats {
    pub captured_fps: u64,
    /// Frames that reached the encoder. Fewer than captured means it fell behind.
    pub to_encoder_fps: u64,
    pub encoded_fps: u64,
    pub sent_kbit_per_second: u64,
    pub longest_capture_gap_ms: u64,
    pub longest_encoded_gap_ms: u64,
    pub longest_send_gap_ms: u64,
}

fn per_second(count: u64, period: Duration) -> u64 {
    let milliseconds = u64::try_from(period.as_millis()).unwrap_or(u64::MAX).max(1);
    count.saturating_mul(1000) / milliseconds
}

fn to_milliseconds(nanoseconds: u64) -> u64 {
    nanoseconds / 1_000_000
}

fn stats_from(
    captured: StageSnapshot,
    to_encoder: StageSnapshot,
    encoded: StageSnapshot,
    sent: StageSnapshot,
    period: Duration,
) -> StreamStats {
    StreamStats {
        captured_fps: per_second(captured.buffers, period),
        to_encoder_fps: per_second(to_encoder.buffers, period),
        encoded_fps: per_second(encoded.buffers, period),
        sent_kbit_per_second: per_second(sent.bytes.saturating_mul(8), period) / 1000,
        longest_capture_gap_ms: to_milliseconds(captured.longest_gap),
        longest_encoded_gap_ms: to_milliseconds(encoded.longest_gap),
        longest_send_gap_ms: to_milliseconds(sent.longest_gap),
    }
}

/// The measuring points of one running pipeline.
pub struct StreamMeter {
    captured: Arc<Stage>,
    to_encoder: Arc<Stage>,
    encoded: Arc<Stage>,
    sent: Arc<Stage>,
    report_every: Duration,
    last_report: Instant,
}

impl StreamMeter {
    /// Attaches to the named elements' pads. A missing element simply stays at
    /// zero, because measuring must never stop the stream.
    pub fn attach(
        pipeline: &gst::Bin,
        capture_queue: &str,
        video_queue: &str,
        sender: &str,
        report_every: Duration,
    ) -> Self {
        let epoch = Instant::now();
        let meter = Self {
            captured: Arc::default(),
            to_encoder: Arc::default(),
            encoded: Arc::default(),
            sent: Arc::default(),
            report_every,
            last_report: epoch,
        };
        let points = [
            (capture_queue, "sink", &meter.captured),
            (capture_queue, "src", &meter.to_encoder),
            (video_queue, "sink", &meter.encoded),
            (sender, "sink", &meter.sent),
        ];
        for (element, pad, stage) in points {
            if let Some(pad) = pipeline
                .by_name(element)
                .and_then(|element| element.static_pad(pad))
            {
                watch(&pad, Arc::clone(stage), epoch);
            }
        }
        meter
    }

    /// The numbers for the period just ended, once one has passed.
    pub fn poll(&mut self) -> Option<StreamStats> {
        let period = self.last_report.elapsed();
        if period < self.report_every {
            return None;
        }
        self.last_report = Instant::now();
        Some(stats_from(
            self.captured.take(),
            self.to_encoder.take(),
            self.encoded.take(),
            self.sent.take(),
            period,
        ))
    }
}

fn watch(pad: &gst::Pad, stage: Arc<Stage>, epoch: Instant) {
    let probe = pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_, info| {
            let bytes = match &info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => buffer.size(),
                Some(gst::PadProbeData::BufferList(list)) => list.calculate_size(),
                _ => 0,
            };
            let now = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
            stage.record(now, u64::try_from(bytes).unwrap_or(u64::MAX));
            gst::PadProbeReturn::Ok
        },
    );
    if probe.is_none() {
        tracing::debug!(pad = %pad.name(), "cannot measure this pad");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_finds_the_longest_gap_and_resets() {
        let stage = Stage::default();
        for (now, bytes) in [
            (1_000_000, 10),
            (34_000_000, 20),
            (400_000_000, 30),
            (410_000_000, 40),
        ] {
            stage.record(now, bytes);
        }
        let first = stage.take();
        assert_eq!(first.buffers, 4);
        assert_eq!(first.bytes, 100);
        assert_eq!(first.longest_gap, 366_000_000);

        let second = stage.take();
        assert_eq!(
            (second.buffers, second.bytes, second.longest_gap),
            (0, 0, 0)
        );
    }

    #[test]
    fn a_stall_across_periods_still_counts() {
        let stage = Stage::default();
        stage.record(1_000_000, 0);
        stage.take();
        stage.record(901_000_000, 0);
        assert_eq!(stage.take().longest_gap, 900_000_000);
    }

    #[test]
    fn stats_are_rates_over_the_period() {
        let snapshot = |buffers, bytes, longest_gap| StageSnapshot {
            buffers,
            bytes,
            longest_gap,
        };
        let stats = stats_from(
            snapshot(300, 0, 40_000_000),
            snapshot(290, 0, 0),
            snapshot(285, 0, 120_000_000),
            snapshot(1000, 5_000_000, 500_000_000),
            Duration::from_secs(5),
        );
        assert_eq!(
            stats,
            StreamStats {
                captured_fps: 60,
                to_encoder_fps: 58,
                encoded_fps: 57,
                sent_kbit_per_second: 8000,
                longest_capture_gap_ms: 40,
                longest_encoded_gap_ms: 120,
                longest_send_gap_ms: 500,
            }
        );
    }

    #[test]
    fn a_zero_period_does_not_divide_by_zero() {
        assert_eq!(per_second(10, Duration::ZERO), 10_000);
    }
}
