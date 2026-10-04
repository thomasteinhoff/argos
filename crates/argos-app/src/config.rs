use serde::{Deserialize, Serialize};

fn default_volume_percent() -> u32 {
    100
}

#[derive(Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
    #[serde(default = "default_volume_percent")]
    pub volume_percent: u32,
}

pub fn load() -> AppConfig {
    confy::load("argos", None).unwrap_or_default()
}

/// Writes the profile, or says why it could not.
///
/// The error used to be discarded, which made a failed save invisible: the user
/// changes a setting, watches it take effect, and finds it reverted on the next
/// launch with nothing to explain it. The likeliest cause is a config directory
/// that cannot be written — a read-only or redirected roaming profile.
pub fn save(config: &AppConfig) -> Result<(), String> {
    confy::store("argos", None, config).map_err(|error| error.to_string())
}
