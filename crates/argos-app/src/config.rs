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

pub fn save(config: &AppConfig) {
    let _ = confy::store("argos", None, config);
}
