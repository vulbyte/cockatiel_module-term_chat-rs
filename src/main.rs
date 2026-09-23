mod audio;
mod config;
mod easing;
mod emoji;
mod engine;
mod fade;
mod images;
mod login;
mod render;
mod types;

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{KeyCode, KeyEvent},
    terminal,
    ExecutableCommand,
};
use tokio::sync::{mpsc, Mutex};
use tracing::{warn, Level};
use tracing_subscriber::FmtSubscriber;

use cockatiel_client::proto::container::Payload;
use futures_util::StreamExt;
use prost::Message as ProstMessage;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use config::ChatConfig;
use easing::EasingQueue;
use engine::EngineHandle;
use fade::{fade_action, FadeAction, FadeMode};
use images::ImageRenderer;
use login::LoginState;
use render::Ui;
use types::{AppStatus, ChatMessageItem, UiMode};

fn build_message_item(
    payload: &Payload,
    emoji_map: &HashMap<String, String>,
    emoji_enabled: bool,
) -> Option<ChatMessageItem> {
    let (platform, content, username, name_color, rank, score, reprimanded, role_badges, user_handle, user_uuid7) =
        match payload {
            Payload::MessagePostProcess(pp) => {
                let raw = pp.raw_message.as_ref();
                let user_data = raw.and_then(|cm| cm.user_data.as_ref());
                let username = user_data
                    .map(|ud| ud.username.clone())
                    .filter(|u| !u.is_empty())
                    .or_else(|| raw.map(|cm| cm.user_uuid7.clone()))
                    .unwrap_or_default();
                let styling = user_data.and_then(|ud| ud.styling.as_ref());
                let name_color = styling
                    .and_then(|st| st.css_properties.get("color"))
                    .cloned()
                    .unwrap_or_default();
                let rank = styling
                    .and_then(|st| st.css_properties.get("rank"))
                    .cloned()
                    .unwrap_or_default();
                let score = styling
                    .and_then(|st| st.css_properties.get("score"))
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let reprimanded = styling
                    .and_then(|st| st.css_properties.get("reprimands"))
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|n| n > 0)
                    .unwrap_or(false);
                let role_badges = user_data.map(role_badges_of).unwrap_or_default();
                let content = if !pp.processed_message.is_empty() {
                    pp.processed_message.clone()
                } else {
                    raw.map(|cm| cm.raw_message.clone()).unwrap_or_default()
                };
                (
                    raw.map(|cm| cm.platform.clone()).unwrap_or_default(),
                    content,
                    username,
                    name_color,
                    rank,
                    score,
                    reprimanded,
                    role_badges,
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                )
            }
            Payload::MessagePreProcess(pre) => {
                let raw = pre.raw_message.as_ref();
                let user_data = raw.and_then(|cm| cm.user_data.as_ref());
                let username = user_data
                    .map(|ud| ud.username.clone())
                    .filter(|u| !u.is_empty())
                    .or_else(|| raw.map(|cm| cm.user_uuid7.clone()))
                    .unwrap_or_default();
                let styling = user_data.and_then(|ud| ud.styling.as_ref());
                let name_color = styling
                    .and_then(|st| st.css_properties.get("color"))
                    .cloned()
                    .unwrap_or_default();
                let rank = styling
                    .and_then(|st| st.css_properties.get("rank"))
                    .cloned()
                    .unwrap_or_default();
                let score = styling
                    .and_then(|st| st.css_properties.get("score"))
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let reprimanded = styling
                    .and_then(|st| st.css_properties.get("reprimands"))
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|n| n > 0)
                    .unwrap_or(false);
                let role_badges = user_data.map(role_badges_of).unwrap_or_default();
                let content = raw.map(|cm| cm.raw_message.clone()).unwrap_or_default();
                (
                    raw.map(|cm| cm.platform.clone()).unwrap_or_default(),
                    content,
                    username,
                    name_color,
                    rank,
                    score,
                    reprimanded,
                    role_badges,
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                )
            }
            _ => return None,
        };

    let content = if emoji_enabled {
        emoji::apply(emoji_map, &content)
    } else {
        content
    };

    Some(ChatMessageItem {
        id: uuid::Uuid::now_v7().to_string(),
        username,
        name_color,
        rank,
        score,
        role_badges,
        reprimanded,
        platform,
        user_handle,
        user_uuid7,
        content,
        image_art: None,
        image_status: None,
        added_at: Instant::now(),
    })
}

fn role_badges_of(ud: &cockatiel_client::proto::UserData) -> String {
    let mut badges = Vec::new();
    if ud.is_owner {
        badges.push("OWNER");
    }
    if ud.is_admin {
        badges.push("ADMIN");
    }
    if ud.is_moderator {
        badges.push("MOD");
    }
    if ud.is_sponsor {
        badges.push("SUB");
    }
    badges.join(" ")
}

fn spawn_image_render(
    config: &ChatConfig,
    renderer: Arc<ImageRenderer>,
    messages: Arc<Mutex<Vec<ChatMessageItem>>>,
    item: ChatMessageItem,
    image_map: Arc<HashMap<String, String>>,
) {
    if config.images_mode != "all" {
        return;
    }
    let urls = images::collect_image_urls(&item.content, &image_map);
    if urls.is_empty() {
        return;
    }
    let id = item.id.clone();
    let min_rank = config.image_min_rank.clone();

    tokio::spawn(async move {
        // Rank gate: only users at or above image_min_rank get embedded images.
        // The threshold can be a rank name (owner/admin/mod/sponsor/opal/gold/
        // silver/regular/coal/trash) OR a numeric score (the user's own trust
        // level, e.g. "20" = only users with score >= 20).
        if !rank_allows(item.score, &item.rank, &min_rank) {
            let mut msgs = messages.lock().await;
            if let Some(m) = msgs.iter_mut().find(|m| m.id == id) {
                m.image_status = Some("rank too low".to_string());
            }
            warn!("[image] not embedding for {}: rank '{}' score {} below '{}'", item.username, item.rank, item.score, min_rank);
            return;
        }

        let mut reason = String::new();
        for url in urls {
            match renderer.render(&url).await {
                images::RenderResult::Ok(art) => {
                    let mut msgs = messages.lock().await;
                    if let Some(m) = msgs.iter_mut().find(|m| m.id == id) {
                        m.image_art = Some(art);
                    }
                    return;
                }
                images::RenderResult::Reason(r) => {
                    reason = r;
                }
            }
        }
        // Every URL failed → show the placeholder with a reason.
        if reason.is_empty() {
            reason = "could not be converted".to_string();
        }
        let mut msgs = messages.lock().await;
        if let Some(m) = msgs.iter_mut().find(|m| m.id == id) {
            m.image_status = Some(reason.clone());
        }
        warn!("[image] {} not shown: {}", item.username, reason);
    });
}

/// Map a rank string to a comparable tier (higher = better).
fn rank_tier(rank: &str) -> i32 {
    match rank.to_ascii_lowercase().as_str() {
        "owner" => 9,
        "admin" => 8,
        "mod" | "moderator" => 7,
        "sponsor" | "sub" => 6,
        "opal" => 5,
        "gold" => 4,
        "silver" => 3,
        "coal" => 1,
        "trash" => 0,
        // "regular" or anything unknown is the baseline.
        _ => 2,
    }
}

/// True when a user (score + rank) is allowed to embed images under `min_rank`,
/// which may be a rank name or a numeric score threshold.
fn rank_allows(score: i64, rank: &str, min_rank: &str) -> bool {
    if let Ok(min_score) = min_rank.trim().parse::<i64>() {
        return score >= min_score;
    }
    rank_tier(rank) >= rank_tier(min_rank)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::WARN)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let config = ChatConfig::load_or_default();
    let emoji_map = if config.emoji_enabled {
        emoji::load_map(&config.emoji_map_path)
    } else {
        HashMap::new()
    };

    // Connect to the engine and split the stream so we can both send and receive.
    let cockatiel = cockatiel_client::CockatielClient::connect("term-chat-rs.json").await?;
    let (write, read) = cockatiel.stream.split();
    let engine = EngineHandle::new(
        cockatiel.auth_token.clone(),
        cockatiel.config.module_name.clone(),
        cockatiel.instance_uuid7.clone(),
        write,
    );

    let messages: Arc<Mutex<Vec<ChatMessageItem>>> = Arc::new(Mutex::new(Vec::new()));
    let pending: Arc<Mutex<EasingQueue>> = Arc::new(Mutex::new(EasingQueue::new(
        config.easing_enabled,
        config.easing_target_per_min,
    )));
    let status: Arc<Mutex<AppStatus>> = Arc::new(Mutex::new(AppStatus::default()));
    let renderer = Arc::new(ImageRenderer::new(
        config.ascii_converter_path.clone(),
        config.ascii_width,
        config.image_referer.clone(),
    ));
    let image_map: Arc<HashMap<String, String>> =
        Arc::new(images::load_map(&config.image_map_path));

    // Read task: engine -> UI.
    {
        let pending = Arc::clone(&pending);
        let status = Arc::clone(&status);
        let result_tx = engine.result_sender();
        let emoji_map = emoji_map.clone();
        let emoji_enabled = config.emoji_enabled;
        let engine_task = engine.clone();
        // Audio playback settings (TTS clips).
        let play_audio = config.play_audio;
        let audio_volume = config.audio_volume;
        let max_audio_seconds = config.max_audio_seconds;
        tokio::spawn(async move {
            let mut read = read;
            loop {
                let Some(msg) = read.next().await else {
                    let mut status_guard = status.lock().await;
                    status_guard.connected = false;
                    status_guard.detail = "disconnected from engine".to_string();
                    break;
                };
                let Ok(WsMessage::Binary(data)) = msg else {
                    continue;
                };
                let Ok(container) = cockatiel_client::proto::Container::decode(data.as_ref()) else {
                    continue;
                };
                let Some(payload) = container.payload else {
                    continue;
                };
                // Answer the engine's liveness probe with our auth token.
                if let Payload::AuthVerify(_) = &payload {
                    let _ = engine_task
                        .send_payload(Payload::AuthVerify(cockatiel_client::proto::AuthVerify {
                            cur_auth: engine_task.auth_token.clone(),
                        }))
                        .await;
                    continue;
                }
                // Acknowledge pipeline messages so the engine advances the chain
                // immediately instead of waiting out the ack timeout.
                let ack_uuid: Option<String> = match &payload {
                    Payload::MessagePreProcess(m) => {
                        if m.message_uuid7.is_empty() { None } else { Some(m.message_uuid7.clone()) }
                    }
                    Payload::MessageInProcess(m) => {
                        if m.message_uuid7.is_empty() { None } else { Some(m.message_uuid7.clone()) }
                    }
                    Payload::MessagePostProcess(m) => {
                        if m.message_uuid7.is_empty() { None } else { Some(m.message_uuid7.clone()) }
                    }
                    _ => None,
                };
                if let Some(u) = ack_uuid {
                    let _ = engine_task.ack_message(&u).await;
                }

                match payload {
                    Payload::DatabaseQueryResult(qr) => {
                        let _ = result_tx.send(qr);
                    }
                    // term-chat is a display-only module, NOT an interactive
                    // surface. Engine/module prompts are deliberately ignored
                    // here so they never interrupt the chat stream — the TUI is
                    // the interface that answers them.
                    Payload::Prompt(_) => {}
                    other => {
                        if let Some(item) = build_message_item(&other, &emoji_map, emoji_enabled) {
                            let mut pending_guard = pending.lock().await;
                            pending_guard.push(item);
                        }
                        // Audio playback (TTS clips): audio created by a
                        // pre/in-process module rides WITH the message; audio
                        // created at the post-process stage is saved to the
                        // timeline and fetched here.
                        if play_audio {
                            if let Payload::MessagePostProcess(pp) = &other {
                                if !pp.audio.is_empty() {
                                    audio::play_audio(pp.audio.clone(), audio_volume, max_audio_seconds);
                                } else if !pp.message_uuid7.is_empty() {
                                    let eng = engine_task.clone();
                                    let uuid = pp.message_uuid7.clone();
                                    tokio::spawn(async move {
                                        audio::fetch_and_play(&eng, &uuid, audio_volume, max_audio_seconds).await;
                                    });
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Enter alternate screen and setup terminal interface.
    let mut stdout = io::stdout();
    stdout.execute(terminal::EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;
    let result = run_tui(
        &mut stdout,
        messages,
        pending,
        status,
        &engine,
        &config,
        renderer,
        image_map,
    )
    .await;

    terminal::disable_raw_mode()?;
    stdout.execute(terminal::LeaveAlternateScreen)?;
    if let Err(err) = result {
        eprintln!("Error in terminal display: {}", err);
    }

    Ok(())
}

/// Returns true when the app should quit.
#[allow(clippy::too_many_arguments)]
async fn run_tui(
    stdout: &mut io::Stdout,
    messages: Arc<Mutex<Vec<ChatMessageItem>>>,
    pending: Arc<Mutex<EasingQueue>>,
    status: Arc<Mutex<AppStatus>>,
    engine: &EngineHandle,
    config: &ChatConfig,
    renderer: Arc<ImageRenderer>,
    image_map: Arc<HashMap<String, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<crossterm::event::Event>();

    tokio::task::spawn_blocking(move || {
        while let Ok(ev) = crossterm::event::read() {
            if event_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut ui = Ui::new();
    let mut login = login::load_or_empty();

    let mut interval = tokio::time::interval(Duration::from_millis(250));
    #[allow(unused_assignments)]
    let mut needs_redraw = true;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                needs_redraw = true;
            }
            maybe_event = event_rx.recv() => {
                match maybe_event {
                    Some(ev) => {
                        if handle_event(ev, &mut ui, &mut login, engine, config, &messages).await? {
                            break;
                        }
                        needs_redraw = true;
                    }
                    None => break,
                }
            }
        }

        // Drain the easing queue into the display buffer (scroll-freeze aware).
        let now = Instant::now();
        let drained = pending.lock().await.poll(now);
        if !drained.is_empty() {
            let added = drained.len();
            let mut msgs = messages.lock().await;
            for mut item in drained {
                spawn_image_render(config, Arc::clone(&renderer), Arc::clone(&messages), item.clone(), Arc::clone(&image_map));
                // The fade clock starts when the message becomes visible.
                item.added_at = now;
                msgs.push(item);
            }
            while msgs.len() > config.max_buffered {
                msgs.remove(0);
            }
            drop(msgs);
            if ui.scroll > 0 {
                ui.scroll += added;
            }
            needs_redraw = true;
        }

        // Fade old messages out of the display buffer on a timer, even when no
        // new messages arrive.
        if config.message_fade_secs > 0 {
            let mode = FadeMode::parse(&config.message_fade_mode);
            let fade_now = Instant::now();
            let mut msgs = messages.lock().await;
            let before = msgs.len();
            msgs.retain(|m| {
                let age = fade_now.saturating_duration_since(m.added_at);
                fade_action(age, config.message_fade_secs, mode) != FadeAction::Remove
            });
            if msgs.len() != before {
                ui.scroll = ui.scroll.min(msgs.len());
                needs_redraw = true;
            }
        }

        if std::mem::take(&mut needs_redraw) {
            let (cols, rows) = terminal::size()?;
            if cols < 10 || rows < 6 {
                continue;
            }
            let msgs = messages.lock().await;
            let status_guard = status.lock().await;
            render::draw(
                stdout,
                cols as usize,
                rows as usize,
                &msgs,
                &ui,
                &status_guard,
                config,
                Some(&login),
            )?;
        }
    }

    Ok(())
}

/// Returns true when the app should quit.
#[allow(clippy::too_many_arguments)]
async fn handle_event(
    ev: crossterm::event::Event,
    ui: &mut Ui,
    login: &mut LoginState,
    engine: &EngineHandle,
    config: &ChatConfig,
    messages: &Arc<Mutex<Vec<ChatMessageItem>>>,
) -> Result<bool, Box<dyn std::error::Error>> {
    match ev {
        crossterm::event::Event::Key(key) => {
            handle_key(key, ui, login, engine, config, messages).await
        }
        crossterm::event::Event::Mouse(m) => {
            match m.kind {
                crossterm::event::MouseEventKind::ScrollUp => {
                    let len = messages.lock().await.len();
                    ui.scroll = (ui.scroll + 3).min(len);
                }
                crossterm::event::MouseEventKind::ScrollDown => {
                    ui.scroll = ui.scroll.saturating_sub(3);
                }
                _ => {}
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Returns true when the app should quit.
async fn handle_key(
    key: KeyEvent,
    ui: &mut Ui,
    login: &mut LoginState,
    engine: &EngineHandle,
    config: &ChatConfig,
    messages: &Arc<Mutex<Vec<ChatMessageItem>>>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let KeyEvent { code, .. } = key;

    // Global quit.
    // Ctrl+C is deliberately NOT intercepted to quit — in a terminal it is the
    // copy shortcut, so quitting on it breaks copy/paste. Use `q` instead.

    match ui.mode {
        UiMode::Chat => match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('l') => {
                ui.mode = UiMode::LoginMenu;
                ui.login_note.clear();
            }
            KeyCode::Char('i') => {
                if login.can_send() {
                    ui.mode = UiMode::Input;
                    ui.draft.clear();
                } else {
                    ui.result_note = "not allowed to send — log in with mod perms".to_string();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let len = messages.lock().await.len();
                ui.scroll = (ui.scroll + 1).min(len);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                ui.scroll = ui.scroll.saturating_sub(1);
            }
            KeyCode::Enter
                if login.can_moderate() => {
                    let len = messages.lock().await.len();
                    let idx = len.saturating_sub(ui.scroll + 1);
                    if idx < len {
                        let msgs = messages.lock().await;
                        let item = &msgs[idx];
                        ui.action_target = Some((
                            item.username.clone(),
                            item.user_handle.clone(),
                            item.platform.clone(),
                            item.user_uuid7.clone(),
                        ));
                        ui.result_note.clear();
                        ui.mode = UiMode::ActionMenu;
                    }
                }
            _ => {}
        },
        UiMode::LoginMenu => match code {
            KeyCode::Char('1') => {
                do_login(engine, ui, login, "twitch", "", config).await;
                ui.mode = UiMode::Chat;
            }
            KeyCode::Char('2') => {
                ui.mode = UiMode::KickHandle;
                ui.kick_handle_draft.clear();
                ui.login_note.clear();
            }
            KeyCode::Char('3') => {
                do_login(engine, ui, login, "youtube", "", config).await;
                ui.mode = UiMode::Chat;
            }
            KeyCode::Char('4') | KeyCode::Esc => {
                ui.mode = UiMode::Chat;
            }
            _ => {}
        },
        UiMode::KickHandle => match code {
            KeyCode::Char(c) => {
                ui.kick_handle_draft.push(c);
            }
            KeyCode::Backspace => {
                ui.kick_handle_draft.pop();
            }
            KeyCode::Enter => {
                let handle = ui.kick_handle_draft.trim().to_string();
                if handle.is_empty() {
                    ui.login_note = "enter your kick username".to_string();
                } else {
                    do_login(engine, ui, login, "kick", &handle, config).await;
                    ui.mode = UiMode::Chat;
                }
            }
            KeyCode::Esc => {
                ui.mode = UiMode::LoginMenu;
            }
            _ => {}
        },
        UiMode::ActionMenu => match code {
            KeyCode::Char('1') => {
                send_mod_action(engine, ui, login, "mod_ban", None).await;
                ui.mode = UiMode::Chat;
            }
            KeyCode::Char('2') => {
                ui.mode = UiMode::TimeoutPrompt;
                ui.timeout_draft.clear();
            }
            KeyCode::Char('3') => {
                send_mod_action(engine, ui, login, "mod_commend", None).await;
                ui.mode = UiMode::Chat;
            }
            KeyCode::Char('4') => {
                send_mod_action(engine, ui, login, "mod_reprimand", None).await;
                ui.mode = UiMode::Chat;
            }
            KeyCode::Esc => {
                ui.mode = UiMode::Chat;
                ui.action_target = None;
            }
            _ => {}
        },
        UiMode::TimeoutPrompt => match code {
            KeyCode::Char(c) => {
                if c.is_ascii_digit() {
                    ui.timeout_draft.push(c);
                }
            }
            KeyCode::Backspace => {
                ui.timeout_draft.pop();
            }
            KeyCode::Enter => {
                let secs: i64 = ui.timeout_draft.parse().unwrap_or(300);
                send_mod_action(engine, ui, login, "mod_timeout", Some(secs)).await;
                ui.mode = UiMode::Chat;
            }
            KeyCode::Esc => {
                ui.mode = UiMode::ActionMenu;
            }
            _ => {}
        },
        UiMode::Input => match code {
            KeyCode::Char(c) => {
                ui.draft.push(c);
            }
            KeyCode::Backspace => {
                ui.draft.pop();
            }
            KeyCode::Tab => {
                let next = match ui.target_platform.as_str() {
                    "twitch" => "kick",
                    "kick" => "youtube",
                    "youtube" => "discord",
                    "discord" => "all",
                    _ => "twitch",
                };
                ui.target_platform = next.to_string();
            }
            KeyCode::Enter => {
                let text = ui.draft.trim().to_string();
                if !text.is_empty() && login.can_send() {
                    let actor = (login.platform.as_str(), login.handle.as_str());
                    match engine.send_message(&ui.target_platform, &text, Some(actor)).await {
                        Ok(()) => ui.result_note = "sent".to_string(),
                        Err(e) => ui.result_note = format!("send failed: {}", e),
                    }
                }
                ui.draft.clear();
                ui.mode = UiMode::Chat;
            }
            KeyCode::Esc => {
                ui.draft.clear();
                ui.mode = UiMode::Chat;
            }
            _ => {}
        },
    }
    Ok(false)
}

async fn do_login(
    engine: &EngineHandle,
    ui: &mut Ui,
    login: &mut LoginState,
    platform: &str,
    kick_handle: &str,
    config: &ChatConfig,
) {
    ui.login_note.clear();
    ui.result_note.clear();

    // For Twitch we need the monitored channel to verify the operator is the
    // owner / a moderator of it.
    let channel = if platform == "twitch" {
        engine
            .adapter_credentials("twitch-adapter")
            .await
            .map(|m| m.get("channel").cloned().unwrap_or_default())
            .unwrap_or_default()
    } else {
        String::new()
    };

    match login::login_and_verify(engine, platform, &channel, kick_handle, config).await {
        Ok(state) => {
            *login = state;
            ui.login_note = format!(
                "logged in as {} {} — {}",
                login.platform,
                login.handle,
                login.effective.label()
            );
        }
        Err(e) => {
            ui.login_note = e;
        }
    }
}

async fn send_mod_action(
    engine: &EngineHandle,
    ui: &mut Ui,
    login: &LoginState,
    query_id: &str,
    duration_secs: Option<i64>,
) {
    let Some((_username, handle, platform, uuid7)) = ui.action_target.clone() else {
        return;
    };
    let mut target = serde_json::json!({
        "platform": platform,
        "handle": handle,
    });
    let looks_like_uuid = uuid7.len() == 36 && uuid7.chars().filter(|c| *c == '-').count() == 4;
    if looks_like_uuid {
        target["uuid7"] = serde_json::json!(uuid7);
    }
    if let Some(secs) = duration_secs {
        target["duration_secs"] = serde_json::json!(secs);
    }

    match engine.mod_action(query_id, &target, login).await {
        Ok(resp) if resp.success => {
            ui.result_note = format!("{} applied", query_id);
        }
        Ok(resp) => {
            ui.result_note = format!("{} failed: {}", query_id, resp.error);
        }
        Err(e) => {
            ui.result_note = format!("{} error: {}", query_id, e);
        }
    }
}