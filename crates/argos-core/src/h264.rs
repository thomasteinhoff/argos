use bytes::Bytes;
use rtc::rtp::header::Header;
use rtc::rtp::packet::Packet;

const FU_A: u8 = 28;

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
        let mut start = self.pos;
        while start + 3 <= self.data.len() {
            if self.data[start] == 0 && self.data[start + 1] == 0 && self.data[start + 2] == 1 {
                break;
            }
            start += 1;
        }
        if start + 3 > self.data.len() {
            self.pos = self.data.len();
            return None;
        }
        let code_len = if start + 4 <= self.data.len() && self.data[start + 3] == 0 {
            4
        } else {
            3
        };
        let body_start = start + code_len;
        let mut end = body_start;
        while end + 3 <= self.data.len() {
            if self.data[end] == 0 && self.data[end + 1] == 0 && self.data[end + 2] == 1 {
                break;
            }
            end += 1;
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
