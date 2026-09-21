use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use openh264::Error;

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
        match self.decoder.decode(access_unit) {
            Ok(Some(frame)) => {
                let (width, height) = frame.dimensions();
                let mut rgba = vec![0u8; width * height * 4];
                frame.write_rgba8(&mut rgba);
                Ok(Some(DecodedFrame {
                    rgba,
                    width: width as u32,
                    height: height as u32,
                }))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(decode_error_message(&error)),
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
    use super::decode_state_flags;

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
