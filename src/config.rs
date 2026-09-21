use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatConfig {
    // Display toggles
    pub show_username: bool,
    pub show_color: bool,
    pub show_platform: bool,
    pub show_rank: bool,
    pub show_role_badges: bool,
    pub disable_custom_colors: bool,
    // Message easing
    pub easing_enabled: bool,
    pub easing_target_per_min: f64,
    // Images (ascii rendering)
    pub images_mode: String, // "none" | "all"
    pub ascii_converter_path: String,
    pub ascii_width: usize,
    /// JSON map of `"token": "image-or-gif URL"` — matched as `:token:` and as a
    /// standalone word; a matched token resolves to a URL that is embedded like
    /// a raw URL in the message.
    #[serde(default = "default_image_map_path")]
    pub image_map_path: String,
    /// Minimum rank tier allowed to show embedded images (owner/admin/mod/
    /// sponsor/opal/gold/silver/regular/coal/trash). Users below this show the
    /// `<image>` placeholder instead. Default "regular" = everyone.
    ///
    /// This can ALSO be a numeric score threshold (the user's own trust level),
    /// e.g. "20" means only users with a user-DB score >= 20 can embed images.
    #[serde(default = "default_min_rank")]
    pub image_min_rank: String,
    /// Optional `Referer` header sent when downloading images (some CDNs only
    /// allow embedding when a Referer is present). Empty = no Referer.
    #[serde(default)]
    pub image_referer: String,
    // Emoji map
    pub emoji_enabled: bool,
    pub emoji_map_path: String,
    pub max_buffered: usize,
    // YouTube OAuth (optional; needed for the Google login flow)
    pub google_oauth_client_id: String,
    pub google_oauth_client_secret: String,
    // Audio playback (TTS clips)
    /// Whether the chat display plays rendered audio at all.
    #[serde(default = "default_play_audio")]
    pub play_audio: bool,
    /// Hard cap on how long a clip is allowed to play (seconds); longer clips
    /// are skipped.
    #[serde(default = "default_max_audio_seconds")]
    pub max_audio_seconds: f64,
    /// Playback volume (0.0–1.0).
    #[serde(default = "default_audio_volume")]
    pub audio_volume: f64,
    // Message fade (roadmap: "disappear after x seconds")
    /// Seconds a message stays on screen before being faded out. `0` = never
    /// fade (default).
    #[serde(default)]
    pub message_fade_secs: u64,
    /// `"remove"` = drop the message outright at the timeout; `"dim"` = dim it
    /// for the final ~2s (or ~20% of the lifetime) then drop it.
    #[serde(default = "default_message_fade_mode")]
    pub message_fade_mode: String,
}

fn default_play_audio() -> bool {
    true
}

fn default_max_audio_seconds() -> f64 {
    15.0
}

fn default_audio_volume() -> f64 {
    0.4
}

fn default_message_fade_mode() -> String {
    "remove".to_string()
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            show_username: true,
            show_color: true,
            show_platform: true,
            show_rank: true,
            show_role_badges: true,
            disable_custom_colors: false,
            easing_enabled: false,
            easing_target_per_min: 120.0,
            images_mode: "none".to_string(),
            ascii_converter_path: "ascii-image-converter".to_string(),
            ascii_width: 60,
            image_map_path: "image_map.json".to_string(),
            image_min_rank: "regular".to_string(),
            image_referer: String::new(),
            emoji_enabled: true,
            emoji_map_path: "emoji_map.json".to_string(),
            max_buffered: 200,
            google_oauth_client_id: String::new(),
            google_oauth_client_secret: String::new(),
            play_audio: true,
            max_audio_seconds: 15.0,
            audio_volume: 0.4,
            message_fade_secs: 0,
            message_fade_mode: "remove".to_string(),
        }
    }
}

fn default_image_map_path() -> String {
    "image_map.json".to_string()
}

fn default_min_rank() -> String {
    "regular".to_string()
}

impl ChatConfig {
    pub fn load_or_default() -> Self {
        Self::load_or_default_from("chat_config.json")
    }

    pub fn load_or_default_from<P: AsRef<Path>>(path: P) -> Self {
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str(&data) {
                return cfg;
            }
        }
        let cfg = Self::default();
        let _ = fs::write(path, serde_json::to_string_pretty(&cfg).unwrap());
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_CONFIG: &str = r#"{
        "show_username": true,
        "show_color": true,
        "show_platform": true,
        "show_rank": true,
        "show_role_badges": true,
        "disable_custom_colors": false,
        "easing_enabled": false,
        "easing_target_per_min": 120.0,
        "images_mode": "none",
        "ascii_converter_path": "ascii-image-converter",
        "ascii_width": 60,
        "emoji_enabled": true,
        "emoji_map_path": "emoji_map.json",
        "max_buffered": 200,
        "google_oauth_client_id": "",
        "google_oauth_client_secret": "",
        "image_map_path": "image_map.json",
        "image_min_rank": "regular",
        "image_referer": ""
    }"#;

    #[test]
    fn old_config_without_fade_keys_parses_with_defaults() {
        // This is the exact shape written before message fade existed.
        let cfg: ChatConfig = serde_json::from_str(MINIMAL_CONFIG).unwrap();
        assert_eq!(cfg.message_fade_secs, 0);
        assert_eq!(cfg.message_fade_mode, "remove");
    }

    #[test]
    fn config_parses_fade_keys() {
        let json = format!(
            "{},\n\"message_fade_secs\": 30,\n\"message_fade_mode\": \"dim\"\n}}",
            &MINIMAL_CONFIG[..MINIMAL_CONFIG.len() - 1]
        );
        let cfg: ChatConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.message_fade_secs, 30);
        assert_eq!(cfg.message_fade_mode, "dim");
    }

    #[test]
    fn default_config_serializes_fade_keys() {
        let cfg = ChatConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["message_fade_secs"], 0);
        assert_eq!(parsed["message_fade_mode"], "remove");
    }
}