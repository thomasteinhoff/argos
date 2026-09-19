use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

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

    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<DecodedFrame>, String> {
        match self
            .decoder
            .decode(access_unit)
            .map_err(|error| error.to_string())?
        {
            Some(frame) => {
                let (width, height) = frame.dimensions();
                let mut rgba = vec![0u8; width * height * 4];
                frame.write_rgba8(&mut rgba);
                Ok(Some(DecodedFrame {
                    rgba,
                    width: width as u32,
                    height: height as u32,
                }))
            }
            None => Ok(None),
        }
    }
}
