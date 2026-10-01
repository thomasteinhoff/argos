//! Lightweight instrumentation shared by the capture, encode, receive and
//! playback pipelines.
//!
//! Everything here is designed to be written from a worker thread and read from
//! the UI thread without contention: durations accumulate into an [`Ema`] via
//! fixed-point atomics, and event counts are plain [`AtomicU64`]s. No mutex is
//! taken on the hot path.
//!
//! The point is to make the pipeline's cost observable. Every stage that can
//! fall behind (capture readback, colour conversion, H.264 encode, RTP write,
//! decode) records its own duration, and every queue that can overflow records
//! how often it dropped. A drop counter climbing while the matching stage's
//! duration climbs points straight at the culprit.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Accumulates a duration sum and a sample count into a pair of atomics, so a
/// reader can derive a mean over the measurement window without resetting state
/// from another thread.
///
/// Fixed-point microseconds keep the accumulation lock-free while staying well
/// inside `u64` for any realistic session (a `u64` of microseconds is ~584,000
/// years). A separate saturating max records the worst single observation,
/// because a session-long mean stays flat during an intermittent stall: a
/// single 400 ms hitch barely moves the total, but it is the single most
/// informative number when hunting exactly that kind of problem.
#[derive(Debug, Default)]
pub struct Ema {
    micros: AtomicU64,
    samples: AtomicU64,
    peak_micros: AtomicU64,
}

impl Ema {
    /// Records one observation.
    pub fn record_micros(&self, micros: u64) {
        self.micros.fetch_add(micros, Ordering::Relaxed);
        self.samples.fetch_add(1, Ordering::Relaxed);
        self.raise_peak(micros);
    }

    /// Records one observation from a duration.
    pub fn record(&self, elapsed: Duration) {
        self.record_micros(elapsed.as_micros() as u64);
    }

    /// Observations recorded in the current window.
    pub fn samples(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }

    /// Total accumulated time in the current window, in microseconds.
    pub fn total_micros(&self) -> u64 {
        self.micros.load(Ordering::Relaxed)
    }

    /// Mean observation in milliseconds, or `0.0` before the first sample.
    pub fn mean_ms(&self) -> f32 {
        let samples = self.samples();
        if samples == 0 {
            return 0.0;
        }
        self.micros.load(Ordering::Relaxed) as f32 / samples as f32 / 1000.0
    }

    /// Worst single observation in the current window, in milliseconds.
    pub fn peak_ms(&self) -> f32 {
        self.peak_micros.load(Ordering::Relaxed) as f32 / 1000.0
    }

    /// Seconds elapsed in the current window, derived from the time the first
    /// observation was recorded. Used to turn a sample count into a rate.
    ///
    /// Returns `None` before the first sample, since there is no baseline yet.
    pub fn window_secs(&self, started: Instant) -> Option<f32> {
        if self.samples() == 0 {
            return None;
        }
        Some(started.elapsed().as_secs_f32().max(1e-3))
    }

    /// Clears the window. Exposed so the UI can offer "reset stats" and turn the
    /// session-long mean into a windowed one on demand, rather than paying for a
    /// ring buffer on the hot path.
    pub fn reset(&self) {
        self.micros.store(0, Ordering::Relaxed);
        self.samples.store(0, Ordering::Relaxed);
        self.peak_micros.store(0, Ordering::Relaxed);
    }

    fn raise_peak(&self, micros: u64) {
        let mut current = self.peak_micros.load(Ordering::Relaxed);
        while micros > current {
            match self.peak_micros.compare_exchange_weak(
                current,
                micros,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

/// A scoped timer that records its elapsed time into an [`Ema`] on drop.
///
/// Use as `let _t = StageTimer::new(&metrics.encode);` so the measurement is
/// taken even if the scope exits through an early return or an error path.
pub struct StageTimer<'a> {
    stage: &'a Ema,
    started: Instant,
}

impl<'a> StageTimer<'a> {
    #[inline]
    pub fn new(stage: &'a Ema) -> Self {
        Self {
            stage,
            started: Instant::now(),
        }
    }
}

impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        self.stage.record(self.started.elapsed());
    }
}

/// A monotonic event counter.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn add(&self, by: u64) {
        self.0.fetch_add(by, Ordering::Relaxed);
    }

    pub fn incr(&self) {
        self.add(1);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Overwrites the counter, for a value that is replaced rather than summed.
    pub fn set(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }

    /// Clears the counter, so a rate can be measured over a known window.
    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

/// Attempts and failures, so a drop rate is always available.
#[derive(Debug, Default)]
pub struct DropCounter {
    frames: AtomicU64,
    dropped: AtomicU64,
}

impl DropCounter {
    /// An attempt that succeeded.
    pub fn record(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// An attempt that was discarded because a consumer was not keeping up.
    pub fn drop_frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Failures as a percentage of attempts, `0.0` before any attempt.
    pub fn drop_percent(&self) -> f32 {
        let frames = self.frames();
        if frames == 0 {
            return 0.0;
        }
        self.dropped() as f32 / frames as f32 * 100.0
    }

    /// Clears both counts, so a rate can be measured over a known window.
    pub fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
    }
}

/// Per-stage timings and drop counters for the capture/encode direction.
///
/// One instance is shared between the capture thread, the encode worker and the
/// UI thread, so every field is a plain atomic.
#[derive(Debug, Default)]
pub struct SenderMetrics {
    /// Time blocked waiting for the capture backend to produce a frame.
    pub acquire: Ema,
    /// Time spent copying the frame out of GPU memory.
    pub readback: Ema,
    /// Time spent converting to I420, including any scaling.
    pub convert: Ema,
    /// Time inside the H.264 encoder.
    pub encode: Ema,
    /// Time spent turning the bitstream into RTP packets.
    pub packetize: Ema,
    /// Time spent handing packets to the transport.
    pub write: Ema,

    /// Frames the backend produced.
    pub captured: DropCounter,
    /// Frames handed to the encoder.
    pub queued: DropCounter,
    /// Frames the encoder turned into a bitstream.
    pub encoded: DropCounter,
    /// Total bitstream bytes.
    pub encoded_bytes: Counter,
    /// Bytes in the most recently encoded keyframe.
    pub last_keyframe_bytes: Counter,
    pub keyframes: Counter,
    /// H.264 encoder errors.
    pub encode_errors: Counter,
    /// RTP writes that failed.
    pub write_errors: Counter,

    /// Opus frames handed to the transport.
    pub audio_frames: Counter,
    /// Encoded audio bytes.
    pub audio_bytes: Counter,
    /// Audio writes that failed.
    pub audio_errors: Counter,
}

/// Per-stage timings and counters for the receive/decode/present direction.
///
/// One instance is shared between the transport's packet callback and the UI
/// thread. Every counter is an atomic, because the callback fires several
/// hundred times a second and locking a shared mutex there competes with the
/// UI thread reading the same numbers.
#[derive(Debug, Default)]
pub struct ReceiverMetrics {
    /// Time spent reassembling NAL units from RTP packets.
    pub depacketize: Ema,
    /// Time inside the H.264 decoder.
    pub decode: Ema,
    /// Time spent expanding the decoded frame to RGBA for display.
    pub present: Ema,
    /// Time spent inside the packet callback in total.
    pub receive: Ema,

    /// Every RTP packet handed to the callback.
    pub packets: Counter,
    /// Video payload bytes received.
    pub bytes: Counter,
    /// Audio payload bytes received.
    pub audio_bytes: Counter,
    pub audio_packets: Counter,
    /// Audio frames handed to the playback device.
    pub audio_decoded: DropCounter,
    pub audio_errors: Counter,
    /// Packets missing between consecutive sequence numbers. The transport has
    /// no retransmission buffer, so each one is a permanently lost packet and
    /// usually means a damaged frame waiting for the next keyframe.
    pub sequence_losses: Counter,

    /// Access units fed to the decoder.
    pub access_units: Counter,
    /// Frames the decoder produced.
    pub decoded: DropCounter,
    /// Decode calls that returned no picture.
    pub no_picture: Counter,
    /// Decode errors; each one means waiting for the next keyframe.
    pub decode_errors: Counter,
    /// Frames that reached the display slot, with replacements counted as
    /// drops. Compare against `decoded` to see whether the UI is behind.
    pub presented: DropCounter,
}

/// Per-stage timings and counters for one audio direction.
///
/// The long-session audio failure this exists to catch is a slow drift, not an
/// event: a jitter buffer that is never corrected empties by degrees until every
/// device period has to be padded with silence. `underruns` climbing while
/// `overruns` stays at zero is that signature, and it is invisible without a
/// counter.
#[derive(Debug, Default)]
pub struct AudioMetrics {
    /// Time inside the WASAPI read or write call.
    pub device_io: Ema,
    /// Time spent converting device bytes into samples.
    pub convert: Ema,
    /// Time inside the Opus encoder or decoder.
    pub codec: Ema,

    /// Device periods served.
    pub periods: Counter,
    /// The jitter buffer was short and silence had to be inserted. The drop
    /// count is the number of periods that contained inserted silence.
    pub underruns: DropCounter,
    /// The jitter buffer overflowed and the oldest audio was discarded.
    pub overruns: DropCounter,
    /// Device errors that were recovered from by rebuilding the client.
    pub device_errors: Counter,
    /// Times the audio thread rebuilt its device client.
    pub restarts: Counter,
    /// Sample-clock drift correction applied, in samples. Non-zero over a long
    /// session is normal and expected; the sign shows which way the clocks run.
    pub drift_samples: Counter,
}

#[cfg(test)]
mod tests {
    use super::{Counter, DropCounter, Ema, StageTimer};
    use std::time::Duration;

    #[test]
    fn empty_stage_reports_zero_not_nan() {
        let stage = Ema::default();
        assert_eq!(stage.samples(), 0);
        assert_eq!(stage.mean_ms(), 0.0);
        assert_eq!(stage.peak_ms(), 0.0);
        assert!(stage.mean_ms().is_finite());
        assert!(stage.peak_ms().is_finite());
    }

    #[test]
    fn mean_is_over_recorded_samples() {
        let stage = Ema::default();
        stage.record(Duration::from_millis(10));
        stage.record(Duration::from_millis(20));
        stage.record(Duration::from_millis(30));
        assert_eq!(stage.samples(), 3);
        assert!((stage.mean_ms() - 20.0).abs() < 0.001);
        assert_eq!(stage.total_micros(), 60_000);
    }

    #[test]
    fn peak_survives_a_single_long_stall() {
        let stage = Ema::default();
        // A stall large enough to be obvious, without a real sleep.
        stage.record_micros(400_000);
        stage.record_micros(4_000);
        stage.record_micros(4_000);
        // The mean stays near 4ms, which is exactly why the peak is tracked.
        assert!((stage.mean_ms() - 136.0).abs() < 0.5);
        assert!((stage.peak_ms() - 400.0).abs() < 0.001);
    }

    #[test]
    fn peak_only_ever_rises() {
        let stage = Ema::default();
        stage.record_micros(500_000);
        stage.record_micros(10_000);
        stage.record_micros(20_000);
        assert!((stage.peak_ms() - 500.0).abs() < 0.001);
    }

    #[test]
    fn reset_clears_the_window() {
        let stage = Ema::default();
        stage.record(Duration::from_millis(10));
        stage.record_micros(900_000);
        stage.reset();
        assert_eq!(stage.samples(), 0);
        assert_eq!(stage.total_micros(), 0);
        assert_eq!(stage.peak_ms(), 0.0);
    }

    #[test]
    fn timer_records_on_drop_even_when_the_scope_ends_early() {
        let stage = Ema::default();
        {
            let _timer = StageTimer::new(&stage);
        }
        assert_eq!(stage.samples(), 1);
    }

    #[test]
    fn drop_counter_reports_ratio() {
        let counter = DropCounter::default();
        assert_eq!(counter.drop_percent(), 0.0);
        for _ in 0..7 {
            counter.record();
        }
        counter.drop_frame();
        counter.drop_frame();
        assert_eq!(counter.frames(), 9);
        assert_eq!(counter.dropped(), 2);
        assert!((counter.drop_percent() - 22.222).abs() < 0.01);
    }

    #[test]
    fn counter_is_monotonic() {
        let counter = Counter::default();
        counter.incr();
        counter.add(41);
        assert_eq!(counter.get(), 42);
    }

    /// `set` must replace, not accumulate: the last-keyframe size is a value
    /// held, so adding the difference reads as a subtraction through a counter.
    #[test]
    fn counter_set_replaces_rather_than_accumulates() {
        let counter = Counter::default();
        counter.set(5000);
        assert_eq!(counter.get(), 5000);
        counter.set(200);
        assert_eq!(counter.get(), 200);
        counter.add(7);
        assert_eq!(counter.get(), 207);
        counter.reset();
        assert_eq!(counter.get(), 0);
    }
}
