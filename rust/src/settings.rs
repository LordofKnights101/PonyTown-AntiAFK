use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Step,
    Wiggle,
    Both,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Step => "Tiny step",
            Mode::Wiggle => "Mouse wiggle",
            Mode::Both => "Both (random)",
        }
    }

    pub fn next(self) -> Mode {
        match self {
            Mode::Step => Mode::Wiggle,
            Mode::Wiggle => Mode::Both,
            Mode::Both => Mode::Step,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub game_url: String,
    pub interval_min: i32,
    pub mode: Mode,
    pub skip_if_active: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            game_url: "https://pony.town".into(),
            interval_min: 4,
            mode: Mode::Step,
            skip_if_active: true,
        }
    }
}

pub fn path() -> PathBuf {
    crate::util::base_dir().join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) {
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path(), json);
    }
}
