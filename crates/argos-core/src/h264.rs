use bytes::Bytes;
use rtc::rtp::header::Header;
use rtc::rtp::packet::Packet;

const FU_A: u8 = 28;

/// Video RTP clock rate (Hz) used for H.264 RTP timestamps.
pub const RTP_CLOCK_RATE: u32 = 90_000;

/// Number of RTP clock ticks per video frame at the given FPS:
/// `RTP_CLOCK_RATE / fps` (3000 at 30 FPS, 1500 at 60 FPS).
pub fn timestamp_interval(fps: u32) -> u32 {
    RTP_CLOCK_RATE / fps.max(1)
}

/// A video RTP timestamp derived from elapsed wall time rather than frame count.
///
/// The counter-derived form (`timestamp += RTP_CLOCK_RATE / fps`) assumes every
/// frame is sent exactly `1/fps` apart. It is not: a frame that is dropped,
/// coalesced, or simply late still advances the counter by a whole interval, so
/// the sender's clock runs fast and the receiver's jitter buffer eventually has
/// to correct by dropping good frames to keep up. A sender stuck at 20 fps with
/// a 30 fps clock is the same defect in slow motion.
///
/// Deriving from the clock keeps playback time proportional to real time. The
/// receiver only needs the deltas between frames, so the epoch is arbitrary and
/// is taken as the first frame; only differences are ever meaningful.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    epoch: std::time::Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            epoch: std::time::Instant::now(),
        }
    }

    /// Ticks elapsed since the first call. Saturates rather than wrapping: a
    /// timestamp that jumps backwards would make the receiver treat the frame as
    /// arriving early and stall its buffer.
    pub fn ticks(&self, at: std::time::Instant) -> u32 {
        let ticks = at
            .saturating_duration_since(self.epoch)
            .as_nanos()
            .saturating_mul(RTP_CLOCK_RATE as u128)
            / 1_000_000_000;
        ticks.min(u32::MAX as u128) as u32
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

pub fn nalu_type(nalu: &[u8]) -> u8 {
    nalu.first().map(|header| header & 0x1f).unwrap_or(0)
}

pub struct AnnexBIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for AnnexBIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let code = next_start_code(self.data, self.pos)?;
        let body_start = code.end;
        let mut end = self.data.len();
        if let Some(next) = next_start_code(self.data, body_start) {
            end = next.start;
        }
        self.pos = end;
        let nalu = &self.data[body_start..end];
        if nalu.is_empty() {
            self.next()
        } else {
            Some(nalu)
        }
    }
}

struct StartCode {
    start: usize,
    end: usize,
}

fn next_start_code(data: &[u8], mut pos: usize) -> Option<StartCode> {
    pos = pos.saturating_sub(1);
    while pos + 3 <= data.len() {
        if data[pos] == 0 && data[pos + 1] == 0 && data[pos + 2] == 1 {
            let four_byte = pos > 0 && data[pos - 1] == 0;
            return Some(StartCode {
                start: if four_byte { pos - 1 } else { pos },
                end: pos + 3,
            });
        }
        pos += 1;
    }
    None
}

pub struct Packetizer {
    pub sequence_number: u16,
    pub ssrc: u32,
    pub payload_type: u8,
    pub mtu: usize,
}

impl Packetizer {
    pub fn new(ssrc: u32, payload_type: u8, mtu: usize) -> Self {
        Self {
            sequence_number: 0,
            ssrc,
            payload_type,
            mtu,
        }
    }

    pub fn packetize(&mut self, nalu: &[u8], timestamp: u32, marker: bool) -> Vec<Packet> {
        if nalu.len() <= self.mtu {
            let header = self.header(timestamp, marker);
            return vec![Packet {
                header,
                payload: Bytes::copy_from_slice(nalu),
            }];
        }

        let nalu_header = nalu[0];
        let fragments: Vec<&[u8]> = nalu[1..].chunks(self.mtu.saturating_sub(2)).collect();
        let count = fragments.len();
        let mut packets = Vec::with_capacity(count);
        for (index, fragment) in fragments.into_iter().enumerate() {
            let start = index == 0;
            let end = index + 1 == count;
            let fu_indicator = (nalu_header & 0xe0) | FU_A;
            let fu_header = (if start { 0x80 } else { 0 })
                | (if end { 0x40 } else { 0 })
                | (nalu_header & 0x1f);
            let mut payload = Vec::with_capacity(fragment.len() + 2);
            payload.push(fu_indicator);
            payload.push(fu_header);
            payload.extend_from_slice(fragment);
            let header = self.header(timestamp, marker && end);
            packets.push(Packet {
                header,
                payload: Bytes::from(payload),
            });
        }
        packets
    }

    fn header(&mut self, timestamp: u32, marker: bool) -> Header {
        let header = Header {
            version: 2,
            padding: false,
            extension: false,
            marker,
            payload_type: self.payload_type,
            sequence_number: self.sequence_number,
            timestamp,
            ssrc: self.ssrc,
            csrc: vec![],
            extension_profile: 0,
            extensions: vec![],
            extensions_padding: 0,
        };
        self.sequence_number = self.sequence_number.wrapping_add(1);
        header
    }
}

#[derive(Default)]
pub struct Depacketizer {
    frame: Vec<Vec<u8>>,
    fragment: Vec<u8>,
    fragment_type: u8,
    ts: Option<u32>,
    sequence: Option<u16>,
    synced: bool,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, packet: &Packet) -> Option<Vec<Vec<u8>>> {
        let ts = packet.header.timestamp;
        let seq = packet.header.sequence_number;
        if self.ts != Some(ts) {
            self.frame.clear();
            self.fragment.clear();
            self.ts = Some(ts);
            self.sequence = None;
        } else if let Some(previous) = self.sequence {
            if previous.wrapping_add(1) != seq {
                self.frame.clear();
                self.fragment.clear();
            }
        }
        self.sequence = Some(seq);

        match packet.payload.first().map(|byte| byte & 0x1f) {
            Some(28) => self.push_fragment(packet),
            Some(_) => self.frame.push(packet.payload.to_vec()),
            None => {}
        }

        if packet.header.marker && !self.frame.is_empty() {
            let access_unit = std::mem::take(&mut self.frame);
            self.ts = None;
            self.sequence = None;
            // Decoding cannot begin until an IDR keyframe arrives: earlier
            // access units (bare SPS/PPS, or P-frames after a loss) fail in
            // OpenH264 with "no parameter sets". After a decode error the
            // caller resets us, which gates re-sync to the next keyframe.
            if access_unit.iter().any(|nalu| nalu_type(nalu) == 5) {
                self.synced = true;
            }
            if !self.synced {
                return None;
            }
            Some(access_unit)
        } else {
            None
        }
    }

    pub fn reset(&mut self) {
        self.frame.clear();
        self.fragment.clear();
        self.ts = None;
        self.sequence = None;
        self.synced = false;
    }

    fn push_fragment(&mut self, packet: &Packet) {
        let Some(&fu_indicator) = packet.payload.first() else {
            return;
        };
        let Some(&fu_header) = packet.payload.get(1) else {
            return;
        };
        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;
        if start {
            self.fragment.clear();
            self.fragment_type = fu_header & 0x1f;
        }
        if packet.payload.len() > 2 {
            self.fragment.extend_from_slice(&packet.payload[2..]);
        }
        if end && !self.fragment.is_empty() {
            let mut nalu = Vec::with_capacity(self.fragment.len() + 1);
            nalu.push((fu_indicator & 0xe0) | self.fragment_type);
            nalu.extend_from_slice(&self.fragment);
            self.fragment.clear();
            self.frame.push(nalu);
        }
    }
}

pub fn access_unit_to_annexb(nalus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nalu in nalus {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nalu);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{timestamp_interval, RTP_CLOCK_RATE};

    #[test]
    fn timestamp_interval_matches_rtp_clock() {
        assert_eq!(RTP_CLOCK_RATE, 90_000);
        assert_eq!(timestamp_interval(30), 3000);
        assert_eq!(timestamp_interval(60), 1500);
    }

    #[test]
    fn timestamp_interval_guards_against_zero_fps() {
        assert_eq!(timestamp_interval(0), RTP_CLOCK_RATE);
    }

    #[test]
    fn clock_advances_at_the_rtp_rate() {
        use super::Clock;
        use std::time::Duration;
        let clock = Clock::new();
        let epoch = clock.epoch;
        // One second of real time is exactly RTP_CLOCK_RATE ticks.
        assert_eq!(clock.ticks(epoch), 0);
        assert_eq!(clock.ticks(epoch + Duration::from_secs(1)), RTP_CLOCK_RATE);
        // A 30 fps frame is 3000 ticks at 33.33 ms.
        let frame = Duration::from_nanos(33_333_333);
        let ticks = clock.ticks(epoch + frame) as u64;
        assert!(
            (ticks as i64 - timestamp_interval(30) as i64).abs() <= 2,
            "one frame advanced {ticks} ticks, expected about 3000"
        );
    }

    /// The defect the clock replaces: a sender that misses frames advances its
    /// timestamp by a whole interval anyway, so its clock outruns real time and
    /// the receiver's buffer has to discard good frames to catch up.
    #[test]
    fn clock_tracks_real_time_when_frames_are_missed() {
        use super::{Clock, RTP_CLOCK_RATE};
        use std::time::Duration;
        let clock = Clock::new();
        let epoch = clock.epoch;

        // A 30 fps stream that only manages 20 fps: ten frames per second, each
        // advancing by the full 30 fps interval.
        let mut counter = 0u32;
        let mut counter_elapsed = Duration::ZERO;
        for frame in 0..20 {
            counter_elapsed += Duration::from_millis(50);
            counter = counter.wrapping_add(RTP_CLOCK_RATE / 30);
            let at = epoch + counter_elapsed;
            assert_eq!(clock.ticks(at), (frame + 1) * 50 * RTP_CLOCK_RATE / 1000);
        }
        // After one second both have advanced 60000 ticks, but the counter
        // believes 20 frames is 20/30 of a second's worth of time — it has
        // silently claimed 666 ms elapsed.
        assert_eq!(counter, 20 * 3000);
        assert_eq!(clock.ticks(epoch + Duration::from_secs(1)), RTP_CLOCK_RATE);
    }

    #[test]
    fn clock_never_moves_backwards() {
        use super::Clock;
        use std::time::Duration;
        let clock = Clock::new();
        // A timestamp that jumps backwards would make the receiver treat the
        // frame as arriving early and stall its buffer.
        let late = clock.epoch + Duration::from_secs(10);
        assert!(clock.ticks(late) >= clock.ticks(clock.epoch));
        assert_eq!(clock.ticks(clock.epoch), 0);
    }

    #[test]
    fn depacketizer_gates_on_keyframe() {
        use super::{Depacketizer, Packetizer};
        let mut packetizer = Packetizer::new(0x5a5a_77e1, 96, 1200);
        let idr = [0x65u8, 0x88, 0x84, 0x01, 0x02, 0x03];
        let p_frame = [0x41u8, 0x9a, 0x01, 0x02, 0x03];

        let mut depacketizer = Depacketizer::new();
        // P-frames are withheld until the first keyframe arrives.
        for packet in packetizer.packetize(&p_frame, 3000, true) {
            assert!(depacketizer.push(&packet).is_none());
        }
        // The first keyframe unlocks the decoder.
        let mut got = None;
        for packet in packetizer.packetize(&idr, 6000, true) {
            got = depacketizer.push(&packet);
        }
        assert!(got.is_some());
        // After the keyframe, P-frames pass through normally...
        let mut got = None;
        for packet in packetizer.packetize(&p_frame, 9000, true) {
            got = depacketizer.push(&packet);
        }
        assert!(got.is_some());
        // ...until a reset (e.g. a decode error) makes the receiver wait for
        // the next keyframe again.
        depacketizer.reset();
        for packet in packetizer.packetize(&p_frame, 12_000, true) {
            assert!(depacketizer.push(&packet).is_none());
        }
    }

    /// A keyframe too big for one packet survives the round trip byte for byte.
    ///
    /// The test above uses six bytes, which never leaves the single-packet path.
    /// Fragmentation is the path every real keyframe takes: a 1080p IDR is tens
    /// of kilobytes against a 1200-byte MTU, so an error here would break every
    /// stream while every existing test still passed.
    #[test]
    fn a_fragmented_keyframe_survives_the_round_trip() {
        use super::{Depacketizer, Packetizer};
        let mut packetizer = Packetizer::new(0x5a5a_77e1, 96, 1200);
        // 0x65 is an IDR slice with nal_ref_idc 3, so it also unlocks the
        // depacketizer's keyframe gate.
        let mut keyframe = vec![0x65u8];
        keyframe.extend((0..20_000u32).map(|index| (index % 251) as u8));

        let mut depacketizer = Depacketizer::new();
        let mut recovered = None;
        for packet in packetizer.packetize(&keyframe, 3000, true) {
            recovered = depacketizer.push(&packet).or(recovered);
        }

        let access_unit = recovered.expect("a fragmented keyframe must reassemble");
        assert_eq!(access_unit.len(), 1);
        assert_eq!(
            access_unit[0], keyframe,
            "FU-A must be transparent to the bytes it carries"
        );
    }

    /// Only the last fragment of a NALU carries the marker.
    ///
    /// The marker is how the receiver learns an access unit is complete, so a
    /// marker on an early fragment hands the decoder a truncated frame — and
    /// every later fragment then looks like the start of a new one.
    #[test]
    fn only_the_last_fragment_carries_the_marker() {
        use super::Packetizer;
        let mut packetizer = Packetizer::new(1, 96, 300);
        let packets = packetizer.packetize(&vec![0x65u8; 1_000], 0, true);
        assert!(packets.len() > 2, "the fixture has to fragment");
        let last = packets.len() - 1;
        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(
                packet.header.marker,
                index == last,
                "fragment {index} of {} has the wrong marker",
                packets.len()
            );
        }
    }

    /// Start and end bits bracket the fragments, and the NALU's own header bits
    /// survive the transformation.
    ///
    /// A start bit set anywhere but the first fragment makes the reassembler
    /// discard everything collected so far, so the frame decodes as its final
    /// fragment alone. The type and NRI bits come back on reassembly, so losing
    /// them here would turn every keyframe into something undecodable.
    #[test]
    fn fragment_bits_bracket_the_payload() {
        use super::Packetizer;
        let mut packetizer = Packetizer::new(1, 96, 300);
        let packets = packetizer.packetize(&vec![0x65u8; 1_000], 0, true);
        let last = packets.len() - 1;

        for (index, packet) in packets.iter().enumerate() {
            let (&indicator, &header) = (&packet.payload[0], &packet.payload[1]);
            assert_eq!(indicator & 0x1f, 28, "fragment {index} is not FU-A");
            assert_eq!(
                indicator & 0xe0,
                0x60,
                "fragment {index} lost the forbidden-zero and NRI bits"
            );
            assert_eq!(
                header & 0x1f,
                5,
                "fragment {index} lost the original NALU type"
            );
            assert_eq!(header & 0x80 != 0, index == 0, "start bit on {index}");
            assert_eq!(header & 0x40 != 0, index == last, "end bit on {index}");
        }
    }

    /// Every fragment of one NALU shares its timestamp and chains sequence
    /// numbers.
    ///
    /// The reassembler throws away the entire access unit when the timestamp
    /// changes and again on a sequence gap, so fragments that disagree on either
    /// are discarded rather than decoded. No single-packet test can see that,
    /// because a one-packet NALU has nothing to disagree with.
    #[test]
    fn fragments_agree_on_the_timestamp_and_the_sequence_numbers() {
        use super::Packetizer;
        let mut packetizer = Packetizer::new(1, 96, 300);
        let packets = packetizer.packetize(&vec![0x65u8; 1_000], 7_500, true);
        assert!(packets.len() > 2, "the fixture has to fragment");
        for packet in &packets {
            assert_eq!(packet.header.timestamp, 7_500);
            assert!(
                packet.payload.len() <= 300,
                "a fragment of {} bytes does not fit the MTU",
                packet.payload.len()
            );
        }
        for pair in packets.windows(2) {
            assert_eq!(
                pair[1].header.sequence_number,
                pair[0].header.sequence_number.wrapping_add(1),
                "sequence numbers must be consecutive or the frame is dropped"
            );
        }
    }

    /// Start codes are found in both lengths, and an empty unit is skipped.
    ///
    /// openh264 writes four-byte codes; a hand-written Annex B stream may use
    /// three. Two codes back to back leave a zero-length unit between them, which
    /// the reassembler cannot type and the decoder cannot accept — so it must not
    /// be yielded as a NALU of its own.
    #[test]
    fn annexb_yields_every_unit_and_skips_the_empty_ones() {
        use super::AnnexBIter;

        let four_byte = [
            0u8, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(
            AnnexBIter::new(&four_byte).collect::<Vec<_>>(),
            vec![
                &[0x67u8, 0x42][..],
                &[0x68u8, 0xce][..],
                &[0x65u8, 0x88][..],
            ]
        );

        let three_byte = [0u8, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65, 0x88];
        assert_eq!(
            AnnexBIter::new(&three_byte).collect::<Vec<_>>(),
            vec![&[0x67u8, 0x42][..], &[0x65u8, 0x88][..]]
        );

        let empty_between = [0u8, 0, 0, 1, 0, 0, 0, 1, 0x65, 0x88];
        assert_eq!(
            AnnexBIter::new(&empty_between).collect::<Vec<_>>(),
            vec![&[0x65u8, 0x88][..]]
        );

        // A trailing unit with no start code after it is still a unit.
        let trailing = [0u8, 0, 0, 1, 0x65, 0x88, 0x99];
        assert_eq!(
            AnnexBIter::new(&trailing).collect::<Vec<_>>(),
            vec![&[0x65u8, 0x88, 0x99][..]]
        );
    }
}
