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
    /// Minimum numeric rank (0-1) allowed to show embedded images. Users whose
    /// rank (the user-db's 0-1 value, exposed as `rank_value` by the engine)
    /// is below this show the `<image>` placeholder instead. Default 0.0 =
    /// everyone. Numbers are for logic; tier NAMES are a display concern from
    /// the root `rank_chart.json`.
    #[serde(default = "default_min_rank")]
    pub image_min_rank: f32,
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
    /// Loopback port for the OAuth redirect listener. Give each module a
    /// distinct port and register the matching redirect URI in the platform
    /// console. Default 3000 (backwards compatible).
    #[serde(default = "default_oauth_redirect_port")]
    pub oauth_redirect_port: u16,
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
    // Reprimand indicator + status/rank colors (roadmap Checkpoint 5).
    /// Show a compact `R` marker on users who have been reprimanded.
    #[serde(default = "default_true")]
    pub show_reprimand: bool,
    /// Color the role badge letters (OWNER/ADMIN/MOD/SUB).
    #[serde(default = "default_true")]
    pub show_status_color: bool,
    /// Color the rank text (e.g. `(opal)`).
    #[serde(default = "default_true")]
    pub show_rank_color: bool,
    // Tuning values (all defaulted; created in config.json when missing).
    /// Audio fetch retries before giving up on a TTS clip.
    #[serde(default = "default_audio_fetch_retries")]
    pub audio_fetch_retries: u32,
    /// Delay between audio fetch retries (ms).
    #[serde(default = "default_audio_fetch_retry_delay_ms")]
    pub audio_fetch_retry_delay_ms: u64,
    /// Hard cap on a single downloaded image's bytes.
    #[serde(default = "default_image_max_bytes")]
    pub image_max_bytes: usize,
    /// Max concurrent image downloads.
    #[serde(default = "default_image_max_concurrent")]
    pub image_max_concurrent: usize,
    /// Per-download HTTP timeout (seconds).
    #[serde(default = "default_image_timeout_secs")]
    pub image_timeout_secs: u64,
    /// Cache eviction bound: ~multiplier × visible chat height.
    #[serde(default = "default_image_cache_multiplier")]
    pub image_cache_multiplier: usize,
    /// Cache eviction floor (entries).
    #[serde(default = "default_image_cache_min")]
    pub image_cache_min: usize,
    /// Fraction of the terminal an embedded image may occupy.
    #[serde(default = "default_image_fit_fraction")]
    pub image_fit_fraction: f64,
    /// Fraction of a message's lifetime used for the dim fade-out window.
    #[serde(default = "default_fade_dim_fraction")]
    pub fade_dim_fraction: f64,
    /// Cap on the dim fade-out window (seconds).
    #[serde(default = "default_fade_dim_max_secs")]
    pub fade_dim_max_secs: u64,
    /// Idle gap (seconds) after which a buffered easing queue flushes.
    #[serde(default = "default_easing_idle_flush_secs")]
    pub easing_idle_flush_secs: u64,
    /// Mouse-wheel scroll step (messages).
    #[serde(default = "default_scroll_step")]
    pub scroll_step: usize,
    /// Deadline for a database query round-trip (seconds).
    #[serde(default = "default_db_query_timeout_secs")]
    pub db_query_timeout_secs: u64,
    /// Capacity of the engine result broadcast channel.
    #[serde(default = "default_query_broadcast_cap")]
    pub query_broadcast_cap: usize,
    /// Default timeout (seconds) when a typed timeout can't be parsed.
    #[serde(default = "default_default_timeout_secs")]
    pub default_timeout_secs: i64,
    /// Initial reconnect backoff (seconds).
    #[serde(default = "default_reconnect_base_secs")]
    pub reconnect_base_secs: u64,
    /// Reconnect backoff cap (seconds).
    #[serde(default = "default_reconnect_max_secs")]
    pub reconnect_max_secs: u64,
    /// HTTP timeout for login API calls (seconds).
    #[serde(default = "default_login_http_timeout_secs")]
    pub login_http_timeout_secs: u64,
    /// How many loopback redirect connections the OAuth listener accepts.
    #[serde(default = "default_oauth_listener_attempts")]
    pub oauth_listener_attempts: u32,
}

fn default_true() -> bool {
    true
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

fn default_oauth_redirect_port() -> u16 {
    3000
}

fn default_audio_fetch_retries() -> u32 {
    3
}

fn default_audio_fetch_retry_delay_ms() -> u64 {
    800
}

fn default_image_max_bytes() -> usize {
    10 * 1024 * 1024
}

fn default_image_max_concurrent() -> usize {
    8
}

fn default_image_timeout_secs() -> u64 {
    15
}

fn default_image_cache_multiplier() -> usize {
    4
}

fn default_image_cache_min() -> usize {
    8
}

fn default_image_fit_fraction() -> f64 {
    0.8
}

fn default_fade_dim_fraction() -> f64 {
    0.2
}

fn default_fade_dim_max_secs() -> u64 {
    2
}

fn default_easing_idle_flush_secs() -> u64 {
    2
}

fn default_scroll_step() -> usize {
    3
}

fn default_db_query_timeout_secs() -> u64 {
    10
}

fn default_query_broadcast_cap() -> usize {
    256
}

fn default_default_timeout_secs() -> i64 {
    300
}

fn default_reconnect_base_secs() -> u64 {
    1
}

fn default_reconnect_max_secs() -> u64 {
    30
}

fn default_login_http_timeout_secs() -> u64 {
    20
}

fn default_oauth_listener_attempts() -> u32 {
    10
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
            image_min_rank: 0.0,
            image_referer: String::new(),
            emoji_enabled: true,
            emoji_map_path: "emoji_map.json".to_string(),
            max_buffered: 200,
            google_oauth_client_id: String::new(),
            google_oauth_client_secret: String::new(),
            oauth_redirect_port: 3000,
            play_audio: true,
            max_audio_seconds: 15.0,
            audio_volume: 0.4,
            message_fade_secs: 0,
            message_fade_mode: "remove".to_string(),
            show_reprimand: true,
            show_status_color: true,
            show_rank_color: true,
            audio_fetch_retries: 3,
            audio_fetch_retry_delay_ms: 800,
            image_max_bytes: 10 * 1024 * 1024,
            image_max_concurrent: 8,
            image_timeout_secs: 15,
            image_cache_multiplier: 4,
            image_cache_min: 8,
            image_fit_fraction: 0.8,
            fade_dim_fraction: 0.2,
            fade_dim_max_secs: 2,
            easing_idle_flush_secs: 2,
            scroll_step: 3,
            db_query_timeout_secs: 10,
            query_broadcast_cap: 256,
            default_timeout_secs: 300,
            reconnect_base_secs: 1,
            reconnect_max_secs: 30,
            login_http_timeout_secs: 20,
            oauth_listener_attempts: 10,
        }
    }
}

fn default_image_map_path() -> String {
    "image_map.json".to_string()
}

fn default_min_rank() -> f32 {
    0.0
}

impl ChatConfig {
    pub fn load_or_default() -> Self {
        Self::load_or_default_from("chat_config.json")
    }

    pub fn load_or_default_from<P: AsRef<Path>>(path: P) -> Self {
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str(&data) {
                // Config convention: settings are created with their default
                // when missing — write any absent keys back so every setting
                // always exists and is editable in place.
                if let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&data) {
                    let mut changed = false;
                    if let Some(obj) = root.as_object_mut() {
                        let defaults = serde_json::json!({
                            "show_reprimand": true,
                            "show_status_color": true,
                            "show_rank_color": true,
                            "audio_fetch_retries": 3,
                            "audio_fetch_retry_delay_ms": 800,
                            "image_max_bytes": 10 * 1024 * 1024,
                            "image_max_concurrent": 8,
                            "image_timeout_secs": 15,
                            "image_cache_multiplier": 4,
                            "image_cache_min": 8,
                            "image_fit_fraction": 0.8,
                            "fade_dim_fraction": 0.2,
                            "fade_dim_max_secs": 2,
                            "easing_idle_flush_secs": 2,
                            "scroll_step": 3,
                            "db_query_timeout_secs": 10,
                            "query_broadcast_cap": 256,
                            "default_timeout_secs": 300,
                            "reconnect_base_secs": 1,
                            "reconnect_max_secs": 30,
                            "login_http_timeout_secs": 20,
                            "oauth_listener_attempts": 10,
                        });
                        if let Some(d) = defaults.as_object() {
                            for (key, value) in d {
                                if !obj.contains_key(key) {
                                    obj.insert(key.clone(), value.clone());
                                    changed = true;
                                }
                            }
                        }
                    }
                    if changed {
                        let _ = fs::write(path, serde_json::to_string_pretty(&root).unwrap());
                    }
                }
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
        "image_min_rank": 0.0,
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