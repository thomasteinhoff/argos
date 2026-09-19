use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
}

pub fn load() -> AppConfig {
    confy::load("argos", None).unwrap_or_default()
}

pub fn save(config: &AppConfig) {
    let _ = confy::store("argos", None, config);
}
