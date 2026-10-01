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
use regex::Regex;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{warn, Level};
use tracing_subscriber::FmtSubscriber;

use cockatiel_client::proto::container_for_engine::Payload as EnginePayload;
use cockatiel_client::proto::container_for_module::Payload as ModulePayload;
use cockatiel_client::proto::*;
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
    payload: &ModulePayload,
    emoji_map: &HashMap<String, String>,
    emoji_regexes: &HashMap<String, Regex>,
    emoji_enabled: bool,
) -> Option<ChatMessageItem> {
    let (platform, content, username, name_color, rank, rank_value, reprimanded, role_badges, user_handle, user_uuid7, message_uuid7, stage) =
        match payload {
            ModulePayload::MessagePostProcess(pp) => {
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
                let rank_value = styling
                    .and_then(|st| st.css_properties.get("rank_value"))
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.0);
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
                    rank_value,
                    reprimanded,
                    role_badges,
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                    pp.message_uuid7.clone(),
                    "post",
                )
            }
            ModulePayload::MessagePreProcess(pre) => {
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
                let rank_value = styling
                    .and_then(|st| st.css_properties.get("rank_value"))
                    .and_then(|s| s.parse::<f32>().ok())
                    .unwrap_or(0.0);
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
                    rank_value,
                    reprimanded,
                    role_badges,
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                    raw.map(|cm| cm.user_uuid7.clone()).unwrap_or_default(),
                    pre.message_uuid7.clone(),
                    "pre",
                )
            }
            _ => return None,
        };

    let content = if emoji_enabled {
        emoji::apply(emoji_map, emoji_regexes, &content)
    } else {
        content
    };

    // Key the item by the engine's message uuid7 so an engine re-delivery
    // (recovery re-queue of the SAME pipeline stage) coalesces into the same
    // item instead of rendering a duplicate. The stage is part of the key
    // because pre-process (raw, "in-progress") and post-process (processed)
    // are distinct, intentional displays of the same message.
    let id = if message_uuid7.is_empty() {
        uuid::Uuid::now_v7().to_string()
    } else {
        format!("{}:{}", stage, message_uuid7)
    };

    Some(ChatMessageItem {
        id,
        username,
        name_color,
        rank,
        rank_value,
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
    image_regexes: Arc<HashMap<String, Regex>>,
) {
    if config.images_mode != "all" {
        return;
    }
    let urls = images::collect_image_urls(&item.content, &image_map, &image_regexes);
    if urls.is_empty() {
        return;
    }
    let id = item.id.clone();
    let min_rank = config.image_min_rank;

    tokio::spawn(async move {
        // Rank gate: only users whose numeric 0-1 rank is at or above
        // image_min_rank get embedded images. Numbers gate; the display NAME
        // (`item.rank`) never does.
        if !rank_allows(item.rank_value, min_rank) {
            let mut msgs = messages.lock().await;
            if let Some(m) = msgs.iter_mut().find(|m| m.id == id) {
                m.image_status = Some("rank too low".to_string());
            }
            warn!("[image] not embedding for {}: rank '{}' (value {:.3}) below '{}'", item.username, item.rank, item.rank_value, min_rank);
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

/// True when a user is allowed to embed images under `image_min_rank`: their
/// numeric 0-1 rank (the engine's `rank_value`) must be >= the threshold.
/// Numbers are for logic; the `rank` display NAME never gates anything.
fn rank_allows(rank_value: f32, min_rank: f32) -> bool {
    rank_value >= min_rank
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
    // Precompile the word-boundary emoji regexes once (they run on the
    // AuthVerify hot path); rebuild whenever the map is reloaded.
    let emoji_regexes = emoji::compile_regexes(&emoji_map);

    // Connect to the engine and split the stream so we can both send and receive.
    let cockatiel = cockatiel_client::CockatielClient::connect("term-chat-rs.json").await?;
    let (write, read) = cockatiel.stream.split();
    let engine = EngineHandle::new(
        cockatiel.auth_token.clone(),
        cockatiel.config.module_name.clone(),
        cockatiel.instance_uuid7.clone(),
        write,
        config.query_broadcast_cap,
        Duration::from_secs(config.db_query_timeout_secs),
    );
    // Shared so the read task can swap in a fresh handle on reconnect while the
    // TUI keeps sending through the CURRENT connection.
    let engine_state: Arc<RwLock<EngineHandle>> = Arc::new(RwLock::new(engine));

    let messages: Arc<Mutex<Vec<ChatMessageItem>>> = Arc::new(Mutex::new(Vec::new()));
    let pending: Arc<Mutex<EasingQueue>> = Arc::new(Mutex::new(EasingQueue::new(
        config.easing_enabled,
        config.easing_target_per_min,
        config.max_buffered,
        config.easing_idle_flush_secs,
    )));
    let status: Arc<Mutex<AppStatus>> = Arc::new(Mutex::new(AppStatus::default()));
    let renderer = Arc::new(ImageRenderer::new(
        config.ascii_converter_path.clone(),
        config.ascii_width,
        config.image_referer.clone(),
        config.image_max_bytes,
        config.image_max_concurrent,
        config.image_timeout_secs,
        config.image_cache_multiplier,
        config.image_cache_min,
        config.image_fit_fraction,
    ));
    let image_map: Arc<HashMap<String, String>> =
        Arc::new(images::load_map(&config.image_map_path));
    let image_regexes: Arc<HashMap<String, Regex>> =
        Arc::new(images::compile_map_regexes(image_map.as_ref()));
    let audio_fetcher =
        audio::AudioFetcher::with_retries(config.audio_fetch_retries, config.audio_fetch_retry_delay_ms);

    // Read task: engine -> UI, with automatic reconnect (exponential backoff).
    {
        let pending = Arc::clone(&pending);
        let messages = Arc::clone(&messages);
        let status = Arc::clone(&status);
        let emoji_map = emoji_map.clone();
        let emoji_regexes = emoji_regexes.clone();
        let emoji_enabled = config.emoji_enabled;
        // Audio playback settings (TTS clips).
        let play_audio = config.play_audio;
        let audio_volume = config.audio_volume;
        let max_audio_seconds = config.max_audio_seconds;
        let reconnect_base = Duration::from_secs(config.reconnect_base_secs);
        let reconnect_max = Duration::from_secs(config.reconnect_max_secs);
        let broadcast_cap = config.query_broadcast_cap;
        let db_query_timeout = Duration::from_secs(config.db_query_timeout_secs);
        let audio_fetcher = audio_fetcher.clone();
        let engine_state = Arc::clone(&engine_state);
        tokio::spawn(async move {
            let mut read: Option<_> = Some(read);
            let mut backoff = reconnect_base;
            loop {
                // The CURRENT engine handle (the read task is the only one that
                // swaps it; the TUI sends through it too).
                let engine = engine_state.read().await.clone();

                let Some(stream) = read.as_mut() else {
                    // Disconnected — reconnect with exponential backoff
                    // (1s, 2s, 4s ... capped at 30s), then re-subscribe by
                    // re-running the engine handshake and swapping in the new
                    // handle (fresh write sink, auth, instance uuid).
                    {
                        let mut status_guard = status.lock().await;
                        status_guard.connected = false;
                        status_guard.detail = format!("reconnecting in {}s...", backoff.as_secs());
                    }
                    tokio::time::sleep(backoff).await;
                    match cockatiel_client::CockatielClient::connect("term-chat-rs.json").await {
                        Ok(client) => {
                            let (write, new_read) = client.stream.split();
                            let new_engine = EngineHandle::new(
                                client.auth_token,
                                client.config.module_name.clone(),
                                client.instance_uuid7,
                                write,
                                broadcast_cap,
                                db_query_timeout,
                            );
                            *engine_state.write().await = new_engine.clone();
                            read = Some(new_read);
                            backoff = reconnect_base;
                            let mut status_guard = status.lock().await;
                            status_guard.connected = true;
                            status_guard.detail = "connected to cockatiel engine".to_string();
                        }
                        Err(e) => {
                            warn!("[engine] reconnect failed: {}", e);
                            backoff = (backoff * 2).min(reconnect_max);
                        }
                    }
                    continue;
                };

                match stream.next().await {
                    None => {
                        // Engine went away — mark disconnected; the next loop
                        // iteration reconnects.
                        let mut status_guard = status.lock().await;
                        status_guard.connected = false;
                        status_guard.detail = "disconnected from engine".to_string();
                        read = None;
                    }
                    Some(Err(e)) => {
                        warn!("[engine] read error: {}", e);
                        let mut status_guard = status.lock().await;
                        status_guard.connected = false;
                        status_guard.detail = "disconnected from engine".to_string();
                        read = None;
                    }
                    Some(Ok(WsMessage::Binary(data))) => {
                        let Ok(container) = ContainerForModule::decode(data.as_ref())
                        else {
                            continue;
                        };
                        let Some(payload) = container.payload else {
                            continue;
                        };
                        // Answer the engine's liveness probe with our auth token.
                        if let ModulePayload::AuthVerify(_) = &payload {
                            let _ = engine
                                .send_payload(EnginePayload::AuthVerify(
                                    cockatiel_client::proto::AuthVerify {
                                        cur_auth: engine.auth_token.clone(),
                                    },
                                ))
                                .await;
                            continue;
                        }
                        // Acknowledge pipeline messages so the engine advances the
                        // chain immediately instead of waiting out the ack timeout.
                        let ack_uuid: Option<String> = match &payload {
                            ModulePayload::MessagePreProcess(m) => {
                                if m.message_uuid7.is_empty() { None } else { Some(m.message_uuid7.clone()) }
                            }
                            ModulePayload::MessageInProcess(m) => {
                                if m.message_uuid7.is_empty() { None } else { Some(m.message_uuid7.clone()) }
                            }
                            ModulePayload::MessagePostProcess(m) => {
                                if m.message_uuid7.is_empty() { None } else { Some(m.message_uuid7.clone()) }
                            }
                            _ => None,
                        };
                        if let Some(u) = ack_uuid {
                            let _ = engine.ack_message(&u).await;
                        }

                        match payload {
                            ModulePayload::DatabaseQueryResult(qr) => {
                                let _ = engine.result_sender().send(qr);
                            }
                            // term-chat is a display-only module, NOT an
                            // interactive surface. Engine/module prompts are
                            // deliberately ignored here so they never interrupt
                            // the chat stream — the TUI is the interface that
                            // answers them.
                            ModulePayload::Prompt(_) => {}
                            other => {
                                if let Some(item) =
                                    build_message_item(&other, &emoji_map, &emoji_regexes, emoji_enabled)
                                {
                                    // The item id is keyed by the engine message uuid7 (per pipeline
                                    // stage), so an engine re-delivery of the
                                    // same stage coalesces: don't push a message
                                    // already shown or already pending.
                                    let already_shown = {
                                        let msgs = messages.lock().await;
                                        msgs.iter().any(|m| m.id == item.id)
                                    };
                                    if !already_shown {
                                        let mut pending_guard = pending.lock().await;
                                        if !pending_guard.contains_id(&item.id) {
                                            pending_guard.push(item);
                                        }
                                    }
                                }
                                // Audio playback (TTS clips): audio created by a
                                // pre/in-process module rides WITH the message;
                                // audio created at the post-process stage is
                                // saved to the timeline and fetched here.
                                if play_audio {
                                    if let ModulePayload::MessagePostProcess(pp) = &other {
                                        if !pp.audio.is_empty() {
                                            audio_fetcher.play_inline(
                                                &pp.message_uuid7,
                                                pp.audio.clone(),
                                                audio_volume,
                                                max_audio_seconds,
                                            );
                                        } else if !pp.message_uuid7.is_empty() {
                                            let eng = engine.clone();
                                            let uuid = pp.message_uuid7.clone();
                                            let fetcher = audio_fetcher.clone();
                                            tokio::spawn(async move {
                                                fetcher
                                                    .fetch_and_play(&eng, &uuid, audio_volume, max_audio_seconds)
                                                    .await;
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        let mut status_guard = status.lock().await;
                        status_guard.connected = false;
                        status_guard.detail = "disconnected from engine".to_string();
                        read = None;
                    }
                    Some(Ok(_)) => {}
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
        &engine_state,
        &config,
        renderer,
        image_map,
        image_regexes,
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
    engine: &Arc<RwLock<EngineHandle>>,
    config: &ChatConfig,
    renderer: Arc<ImageRenderer>,
    image_map: Arc<HashMap<String, String>>,
    image_regexes: Arc<HashMap<String, Regex>>,
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
                spawn_image_render(
                    config,
                    Arc::clone(&renderer),
                    Arc::clone(&messages),
                    item.clone(),
                    Arc::clone(&image_map),
                    Arc::clone(&image_regexes),
                );
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
                fade_action(age, config.message_fade_secs, mode, config.fade_dim_fraction, config.fade_dim_max_secs) != FadeAction::Remove
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
    engine: &Arc<RwLock<EngineHandle>>,
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
                    ui.scroll = (ui.scroll + config.scroll_step).min(len);
                }
                crossterm::event::MouseEventKind::ScrollDown => {
                    ui.scroll = ui.scroll.saturating_sub(config.scroll_step);
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
    engine: &Arc<RwLock<EngineHandle>>,
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
                let secs: i64 = ui.timeout_draft.parse().unwrap_or(config.default_timeout_secs);
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
                    // Send through the CURRENT engine handle (may have been
                    // swapped by a reconnect).
                    let eng = engine.read().await.clone();
                    match eng.send_message(&ui.target_platform, &text, Some(actor)).await {
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
    engine: &Arc<RwLock<EngineHandle>>,
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
        let eng = engine.read().await.clone();
        eng.adapter_credentials("twitch-adapter")
            .await
            .map(|m| m.get("channel").cloned().unwrap_or_default())
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Clone the current handle so the (long) OAuth flow doesn't hold the
    // engine lock and block a reconnect swap.
    let eng = engine.read().await.clone();
    match login::login_and_verify(&eng, platform, &channel, kick_handle, config).await {
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
    engine: &Arc<RwLock<EngineHandle>>,
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

    let eng = engine.read().await.clone();
    match eng.mod_action(query_id, &target, login).await {
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