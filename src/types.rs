use std::time::Instant;

#[derive(Clone, Debug)]
pub struct ChatMessageItem {
    pub id: String,
    pub username: String,
    pub name_color: String,
    pub rank: String,
    /// Raw user score (when the engine enriched the user); used for custom
    /// trust-level thresholds (e.g. `image_min_rank` as a number).
    pub score: i64,
    pub role_badges: String,
    pub platform: String,
    /// Raw identifier the adapter supplied (usually the platform handle).
    pub user_handle: String,
    /// Enriched user DB uuid7 (when the engine resolved the user).
    pub user_uuid7: String,
    pub content: String,
    /// Rendered ascii art for an image in the message (async, may fill in later).
    pub image_art: Option<String>,
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

/// An unanswered prompt (e.g. an audit review) broadcast by the engine.
pub struct PromptData {
    pub prompt: cockatiel_client::proto::Prompt,
    pub deadline: std::time::Instant,
    /// Typed text for free-text prompts (`prompt.input_label` is non-empty).
    pub text_input: String,
}