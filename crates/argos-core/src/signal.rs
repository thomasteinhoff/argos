use base64::Engine;
use rtc::peer_connection::sdp::RTCSessionDescription;

pub fn encode(description: &RTCSessionDescription) -> String {
    let json = serde_json::to_string(description).expect("serialize session description");
    base64::engine::general_purpose::STANDARD.encode(json)
}

pub fn decode(packed: &str) -> Result<RTCSessionDescription, String> {
    let json = base64::engine::general_purpose::STANDARD
        .decode(packed)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&json).map_err(|error| error.to_string())
}
