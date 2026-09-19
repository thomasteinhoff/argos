use openh264::encoder::Encoder;
use openh264::formats::{RgbaSliceU8, YUVBuffer};

pub struct H264Encoder {
    encoder: Encoder,
}

impl H264Encoder {
    pub fn new() -> Result<Self, String> {
        let encoder = Encoder::new().map_err(|error| error.to_string())?;
        Ok(Self { encoder })
    }

    pub fn encode(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
        if width % 2 != 0 || height % 2 != 0 {
            return Err("frame dimensions must be even".to_string());
        }
        let source = RgbaSliceU8::new(rgba, (width as usize, height as usize));
        let yuv = YUVBuffer::from_rgba8_source(source);
        let bitstream = self.encoder.encode(&yuv).map_err(|error| error.to_string())?;
        Ok(bitstream.to_vec())
    }

    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }
}