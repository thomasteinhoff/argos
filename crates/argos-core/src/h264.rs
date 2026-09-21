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
}
