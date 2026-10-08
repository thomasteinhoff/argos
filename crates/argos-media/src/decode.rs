use std::sync::{Arc, Mutex};

use argos_core::h264;
use argos_core::metrics::{ReceiverMetrics, StageTimer};
use argos_core::session;
use argos_core::Packet;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use openh264::Error;

use crate::audio::{AudioPlayback, FirstError, OpusAudioDecoder};

pub struct DecodedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct H264Decoder {
    decoder: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self, String> {
        let decoder = Decoder::new().map_err(|error| error.to_string())?;
        Ok(Self { decoder })
    }

    pub fn decode(
        &mut self,
        access_unit: &[u8],
        metrics: &ReceiverMetrics,
    ) -> Result<Option<DecodedFrame>, String> {
        // The H.264 decode and the YUV-to-RGBA expansion are timed separately.
        // `write_rgba8` touches all width*height*4 output bytes, so at 1080p it
        // costs about as much as the decode itself, and allocating that buffer
        // per frame is a third cost hiding between them. Different fixes need
        // different numbers, so they are measured apart.
        let decoded = {
            let _decode = StageTimer::new(&metrics.decode);
            self.decoder
                .decode(access_unit)
                .map_err(|error| decode_error_message(&error))?
        };
        let Some(frame) = decoded else {
            return Ok(None);
        };
        let (width, height) = frame.dimensions();
        let mut rgba = vec![0u8; width * height * 4];
        {
            let _present = StageTimer::new(&metrics.present);
            frame.write_rgba8(&mut rgba);
        }
        metrics.decoded.record();
        Ok(Some(DecodedFrame {
            rgba,
            width: width as u32,
            height: height as u32,
        }))
    }
}

/// Sequence-slot bookkeeping for the video packet stream.
///
/// Kept out of `DecodeSink` as its own type because the loss accounting is the
/// one piece of the receive path with real branch logic, and it earns a test.
/// The slot is internal: the UI never reads these numbers, they exist only to
/// feed `ReceiverMetrics::sequence_losses`.
#[derive(Default)]
struct SequenceTracker {
    highest: Option<u16>,
}

impl SequenceTracker {
    /// Packets lost between `seq` and the previous one: 0 for the first packet,
    /// an in-order follow-up, or an out-of-order or duplicate packet (which is
    /// not *more* loss). A forward jump larger than half the sequence space is
    /// treated as out-of-order, matching RTP's reorder heuristic.
    fn lost(&mut self, seq: u16) -> u64 {
        match self.highest {
            None => {
                self.highest = Some(seq);
                0
            }
            Some(highest) => {
                if seq != highest && seq.wrapping_sub(highest) < 0x8000 {
                    self.highest = Some(seq);
                    seq.wrapping_sub(highest) as u64 - 1
                } else {
                    0
                }
            }
        }
    }
}

/// The receive side of one viewer session: depacketize, demux, decode, and
/// hand frames to the presentation slot — all of it inside the transport's
/// packet callback.
///
/// This used to live in the app's UI file, closed over by an anonymous
/// closure. Moving it here put the pipeline logic next to the codecs that
/// serve it, made the loss accounting unit-testable, and left the UI file
/// with nothing but the five-line packet callback glue.
pub struct DecodeSink {
    depacketizer: h264::Depacketizer,
    decoder: Option<H264Decoder>,
    audio_decoder: Option<OpusAudioDecoder>,
    /// The frame the UI presents on screen. `None` means the UI has consumed
    /// the previous one, so a new frame is presented; `Some` means the last
    /// one is stale by definition and the replacement is counted as a drop.
    latest: Arc<Mutex<Option<DecodedFrame>>>,
    /// Where decoded audio goes. `None` when the playback thread could not
    /// start; audio packets are then simply not decoded.
    playback: Option<Arc<AudioPlayback>>,
    /// Counters and stage timings shared with the UI thread. Every counter is
    /// an atomic because this code runs hundreds of times a second while the
    /// UI reads the same numbers.
    metrics: Arc<ReceiverMetrics>,
    tracker: SequenceTracker,
    /// First decode failure, kept since the first one is the informative one.
    video_error: FirstError,
    /// First audio decode failure, kept for the same reason.
    audio_error: FirstError,
}

impl DecodeSink {
    pub fn new(
        latest: Arc<Mutex<Option<DecodedFrame>>>,
        playback: Option<Arc<AudioPlayback>>,
        metrics: Arc<ReceiverMetrics>,
    ) -> Self {
        Self {
            depacketizer: h264::Depacketizer::new(),
            decoder: H264Decoder::new().ok(),
            audio_decoder: OpusAudioDecoder::new().ok(),
            latest,
            playback,
            metrics,
            tracker: SequenceTracker::default(),
            video_error: FirstError::default(),
            audio_error: FirstError::default(),
        }
    }

    /// First decode error, if any, for the diagnostics panel.
    pub fn first_video_error(&self) -> Option<String> {
        self.video_error.peek()
    }

    /// First audio decode error, if any, for the diagnostics panel.
    pub fn first_audio_error(&self) -> Option<String> {
        self.audio_error.peek()
    }

    /// Handles one RTP packet. Called from the transport's listener thread.
    pub fn on_packet(&mut self, packet: &Packet) {
        // The receive timer is a drop guard, so it holds a borrow of its
        // metrics for the whole callback. Copied to a local — a refcount bump
        // — so the stage handlers below can take `&mut self`.
        let metrics = self.metrics.clone();
        // Total time inside the callback. If this mean approaches the packet
        // arrival interval, the callback is the bottleneck and the receiver
        // cannot keep up no matter how much spare CPU the decoder has.
        let _receive = StageTimer::new(&metrics.receive);
        // Counters are atomics rather than a shared mutex: at 500+ video
        // packets a second this callback used to take that lock three or four
        // times per packet while the UI thread read it every frame, and the
        // contention was itself a source of jitter.
        metrics.packets.incr();
        metrics.bytes.add(packet.payload.len() as u64);
        if packet.header.payload_type == session::AUDIO_PT {
            self.on_audio(packet);
        } else {
            self.on_video(packet);
        }
    }

    fn on_audio(&mut self, packet: &Packet) {
        self.metrics.audio_packets.incr();
        self.metrics.audio_bytes.add(packet.payload.len() as u64);
        let Some(playback) = &self.playback else {
            return;
        };
        let Some(decoder) = self.audio_decoder.as_mut() else {
            return;
        };
        match decoder.decode(&packet.payload) {
            Ok(samples) => {
                playback.push(samples);
                self.metrics.audio_decoded.record();
            }
            Err(error) => {
                self.metrics.audio_errors.incr();
                self.audio_error.set(error);
            }
        }
    }

    fn on_video(&mut self, packet: &Packet) {
        let lost = self.tracker.lost(packet.header.sequence_number);
        if lost > 0 {
            // A gap here means the packet never arrived, and with no
            // retransmission buffer in the transport the frame it belonged to
            // is unrecoverable. This is the number that explains a stalled
            // viewer.
            self.metrics.sequence_losses.add(lost);
        }
        let nalus = {
            let _depacketize = StageTimer::new(&self.metrics.depacketize);
            self.depacketizer.push(packet)
        };
        let Some(nalus) = nalus else {
            return;
        };
        self.metrics.access_units.incr();
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        let access_unit = h264::access_unit_to_annexb(&nalus);
        match decoder.decode(&access_unit, &self.metrics) {
            Ok(Some(frame)) => {
                // Remember the shape this session started with. The sharer can
                // drop its resolution mid-stream, and the diagnostics panel
                // wants the *first* frame's dimensions, not the current ones.
                if self.metrics.first_width.get() == 0 {
                    self.metrics.first_width.set(frame.width as u64);
                    self.metrics.first_height.set(frame.height as u64);
                }
                // Only the newest frame matters. If the UI has not consumed
                // the previous one, it is stale by definition, so replacing it
                // is the correct behaviour and the drop is counted so the
                // present rate can be compared against the decode rate.
                if let Ok(mut slot) = self.latest.lock() {
                    if slot.is_some() {
                        self.metrics.presented.drop_frame();
                    } else {
                        self.metrics.presented.record();
                    }
                    *slot = Some(frame);
                }
            }
            Ok(None) => self.metrics.no_picture.incr(),
            Err(error) => {
                // The decoder lost sync (e.g. a keyframe was lost): drop
                // everything until the next keyframe rather than feeding it
                // error-prone frames; the depacketizer's sync gate handles
                // that once reset.
                self.depacketizer.reset();
                self.metrics.decode_errors.incr();
                self.video_error.set(error);
            }
        }
    }
}

/// OpenH264 reports decode problems as a bitmask of `DECODING_STATE` flags
/// surfaced through `Error::native_code()`, e.g. `20` == `dsBitstreamError`
/// (4) | `dsNoParamSets` (16): a corrupt access unit decoded with no SPS/PPS
/// available. Turn it into something readable so the app's "First decode
/// error:" line is actionable instead of a bare native number.
fn decode_error_message(error: &Error) -> String {
    let code = error.native_code();
    let flags = decode_state_flags(code as i32);
    if flags.is_empty() {
        format!("OpenH264 decode failed: native error {code}")
    } else {
        format!(
            "OpenH264 decode failed: state {code} ({})",
            flags.join(" | ")
        )
    }
}

/// Known `DECODING_STATE` flags (from OpenH264's codec_def.h); unknown bits
/// fall through so the number stays visible.
fn decode_state_flags(code: i32) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if code & 1 != 0 {
        flags.push("dsFramePending");
    }
    if code & 2 != 0 {
        flags.push("dsRefLost");
    }
    if code & 4 != 0 {
        flags.push("dsBitstreamError");
    }
    if code & 16 != 0 {
        flags.push("dsNoParamSets");
    }
    if code & 32 != 0 {
        flags.push("dsDataErrorConcealed");
    }
    if code & 64 != 0 {
        flags.push("dsRefListNullPtrs");
    }
    if code & 4096 != 0 {
        flags.push("dsInvalidArgument");
    }
    if code & 16_384 != 0 {
        flags.push("dsOutOfMemory");
    }
    if code & 32_768 != 0 {
        flags.push("dsDstBufNeedExpan");
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::{decode_state_flags, SequenceTracker};

    /// Loss accounting is the one branch in the receive path worth a focused
    /// test: a wrong gap count poisons `sequence_losses`, and through the
    /// quality controller, the sharer's whole resolution decision.
    #[test]
    fn counts_lost_packets_between_sequence_gaps() {
        let mut tracker = SequenceTracker::default();
        assert_eq!(tracker.lost(10), 0); // first packet anchors the slot
        assert_eq!(tracker.lost(11), 0); // in order
        assert_eq!(tracker.lost(11), 0); // duplicate, not more loss
        assert_eq!(tracker.lost(14), 2); // two packets missing in the gap
        assert_eq!(tracker.lost(13), 0); // reordered, keeps the high slot
                                         // A forward jump beyond half the sequence space is a reorder, not a
                                         // 65k-strong burst of loss: the stream restarted rather than lost
                                         // half the space.
        assert_eq!(tracker.lost(65_000), 0);
    }

    /// 0 after 65535 is the adjacent next packet, and a gap after it still
    /// counts: the sequence counter wraps without resetting the accounting.
    #[test]
    fn wrapping_sequence_stays_ordered() {
        let mut tracker = SequenceTracker::default();
        tracker.lost(65_533);
        assert_eq!(tracker.lost(65_534), 0); // in order near the top
        assert_eq!(tracker.lost(65_535), 0);
        assert_eq!(tracker.lost(0), 0); // 0 after 65535 is the next packet
        assert_eq!(tracker.lost(5), 4); // a real gap counts again after the wrap
    }

    #[test]
    fn maps_bitstream_error_with_no_param_sets() {
        // OpenH264 returned 20 on the reported receiver failure:
        // dsBitstreamError (4) | dsNoParamSets (16).
        assert_eq!(
            decode_state_flags(20),
            vec!["dsBitstreamError", "dsNoParamSets"]
        );
    }

    #[test]
    fn maps_single_flags_and_ignores_unknown_bits() {
        assert_eq!(decode_state_flags(4), vec!["dsBitstreamError"]);
        assert_eq!(decode_state_flags(16), vec!["dsNoParamSets"]);
        assert_eq!(decode_state_flags(2), vec!["dsRefLost"]);
        assert_eq!(decode_state_flags(0x800), Vec::<&str>::new());
    }
}
