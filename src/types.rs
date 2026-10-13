use std::time::Instant;

#[derive(Clone, Debug)]
pub struct ChatMessageItem {
    pub id: String,
    pub username: String,
    pub name_color: String,
    pub rank: String,
    /// Numeric 0-1 rank (the engine's `rank_value` css property). Numbers are
    /// for logic — `image_min_rank` compares against this; `rank` (the name)
    /// is for display only.
    pub rank_value: f32,
    /// The user's score (points), from the engine's `score` css property.
    /// Displayed beside the name when `show_score` is on.
    pub score: i32,
    /// The user's current points multiplier for this stream, from the engine's
    /// `multiplier` css property (combined f32, 1.0 baseline up to 4.0).
    /// Rendered as `x1.4` ahead of the score only when greater than 1.0.
    pub multiplier: f32,
    pub role_badges: String,
    /// True when the engine flagged this message as a donation (superchat,
    /// membership, gift, ...). Drives the donation badge before the content.
    pub is_donation: bool,
    /// Human-readable donation amount (e.g. `$5.00`). Empty for donations with
    /// no money attached (plain memberships/gifts).
    pub donation_amount: String,
    /// Donation kind (e.g. `superchat`, `membership`, `gift`), display-cased
    /// by the renderer.
    pub donation_kind: String,
    /// True when the user has been reprimanded (reprimands counter > 0);
    /// drives the compact `R` indicator.
    pub reprimanded: bool,
    pub platform: String,
    /// Raw identifier the adapter supplied (usually the platform handle).
    pub user_handle: String,
    /// Enriched user DB uuid7 (when the engine resolved the user).
    pub user_uuid7: String,
    pub content: String,
    /// Rendered ascii art for an image in the message (async, may fill in later).
    pub image_art: Option<String>,
    /// Ascii frames for an animated image (GIF/APNG), played by the renderer.
    /// `image_art` stays `None` for animated images; `image_frames` drives them.
    pub image_frames: Option<Vec<String>>,
    /// Per-frame durations (ms) matching `image_frames` (u32 for 32-bit systems).
    pub image_frame_delays_ms: Option<Vec<u32>>,
    /// Why an image was NOT shown (e.g. "rank too low", "embedding not allowed",
    /// "conversion failed"). `None` = no image attempted, or rendered fine.
    /// The renderer shows a plain `<image>` placeholder when this is Some.
    pub image_status: Option<String>,
    /// When the message entered the display buffer — the start of the fade
    /// clock for `message_fade_secs`.
    pub added_at: Instant,
}

#[derive(Clone, Debug)]
pub struct AppStatus {
    pub connected: bool,
    pub detail: String,
}

impl Default for AppStatus {
    fn default() -> Self {
        Self {
            connected: true,
            detail: "connected to cockatiel engine".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiMode {
    Chat,
    LoginMenu,
    ActionMenu,
    Input,
    KickHandle,
    TimeoutPrompt,
}
