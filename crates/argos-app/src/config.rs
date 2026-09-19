use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
        }
    }
}

pub fn load() -> AppConfig {
    confy::load("argos", None).unwrap_or_default()
}

pub fn save(config: &AppConfig) {
    let _ = confy::store("argos", None, config);
}