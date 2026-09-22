use crossterm::cursor;
use crossterm::terminal::{self, ClearType};
use crossterm::ExecutableCommand;
use std::io::{self, Write};
use std::time::Instant;

use crate::config::ChatConfig;
use crate::fade::{fade_action, FadeAction, FadeMode};
use crate::login::LoginState;
use crate::types::{AppStatus, ChatMessageItem, UiMode};

pub const HEADER: &str = " Cockatiel Term Chat ";

/// Interactive UI state that the event loop mutates and the renderer draws.
pub struct Ui {
    pub mode: UiMode,
    /// Offset from the bottom of the message buffer. 0 = bottom (auto-scroll).
    pub scroll: usize,
    pub draft: String,
    pub target_platform: String,
    pub kick_handle_draft: String,
    pub timeout_draft: String,
    pub action_target: Option<(String, String, String, String)>, // username, handle, platform, uuid7
    pub login_note: String,
    pub result_note: String,
}

impl Ui {
    pub fn new() -> Self {
        Self {
            mode: UiMode::Chat,
            scroll: 0,
            draft: String::new(),
            target_platform: "all".to_string(),
            kick_handle_draft: String::new(),
            timeout_draft: String::new(),
            action_target: None,
            login_note: String::new(),
            result_note: String::new(),
        }
    }
}

pub struct RenderedLine {
    pub username: String,
    pub name_color: String,
    pub bracket_content: String,
    pub content: String,
    pub image_art: Option<String>,
    pub image_status: Option<String>,
    /// True when the message is inside the fade-out window (`dim` mode).
    pub dimmed: bool,
}

/// Build the bracket content (everything inside `[...]`) honoring display toggles.
fn bracket_content(
    msg: &ChatMessageItem,
    config: &ChatConfig,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    let mut user_bit = String::new();
    if config.show_username && !msg.username.is_empty() {
        user_bit = msg.username.clone();
    }
    if config.show_rank && !msg.rank.is_empty() && msg.rank != "regular"
        && !user_bit.is_empty() {
            user_bit.push_str(&format!(" ({})", msg.rank));
        }
    if config.show_role_badges && !msg.role_badges.is_empty() {
        if !user_bit.is_empty() {
            user_bit.push(' ');
        }
        user_bit.push_str(&msg.role_badges);
    }
    if !user_bit.is_empty() {
        parts.push(user_bit);
    }

    if config.show_platform && !msg.platform.is_empty() {
        let pl_code: String = if msg.platform.chars().count() > 2 {
            msg.platform.chars().take(2).collect()
        } else {
            msg.platform.clone()
        };
        parts.push(pl_code);
    }

    parts.join(" | ")
}

fn ceil_div(len: usize, width: usize) -> usize {
    len.max(1).div_ceil(width)
}

fn rows_needed(line: &RenderedLine, cols: usize) -> usize {
    let bracket_rows = ceil_div(line.bracket_content.chars().count(), cols);
    let content_rows = ceil_div(line.content.chars().count(), cols);
    let image_rows = line
        .image_art
        .as_ref()
        .map(|art| art.lines().map(|l| ceil_div(l.chars().count(), cols).max(1)).sum())
        .unwrap_or_else(|| {
            // A failed/blocked image still takes a single `<image>` row.
            if line.image_status.is_some() {
                1
            } else {
                0
            }
        });
    bracket_rows + content_rows + image_rows + 1 // +1 spacer
}

/// Choose which messages to show given the scroll offset. Returns (start_index,
/// slice_len) where start is the index of the OLDEST message to consider and
/// count is how many messages from `start` up to the newest. `scroll` skips the
/// newest `scroll` messages (they stay frozen at the bottom); 0 = auto-scroll
/// to the newest, and the renderer trims old messages by row budget.
fn visible_range(len: usize, scroll: usize) -> (usize, usize) {
    let skip_bottom = scroll.min(len);
    let count = len.saturating_sub(skip_bottom);
    (0, count)
}

/// Draw the full frame. Returns io::Result; caller holds the message/status locks.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    stdout: &mut io::Stdout,
    cols: usize,
    rows: usize,
    messages: &[ChatMessageItem],
    ui: &Ui,
    status: &AppStatus,
    config: &ChatConfig,
    login: Option<&LoginState>,
) -> io::Result<()> {
    stdout.execute(cursor::MoveTo(0, 0))?;
    stdout.execute(terminal::Clear(ClearType::All))?;

    let cols_usize = cols;

    // 1. Header
    let padded_header = format!("{:^width$}", HEADER, width = cols_usize);
    print!("\x1b[44m\x1b[37m{}\x1b[0m\r\n", padded_header);

    // 2. Message area
    let max_chat_rows = rows.saturating_sub(3);
    let (start, count) = visible_range(messages.len(), ui.scroll);

    // Build rendered lines for the visible window, walking backwards from the
    // newest visible message and truncating to the row budget.
    let mut selected: Vec<RenderedLine> = Vec::new();
    let mut used_rows = 0usize;
    for idx in (start..start + count).rev() {
        let msg = &messages[idx];
        let age = Instant::now().saturating_duration_since(msg.added_at);
        let fade_action = fade_action(
            age,
            config.message_fade_secs,
            FadeMode::parse(&config.message_fade_mode),
        );
        let dimmed = fade_action == FadeAction::Dim;
        let line = RenderedLine {
            username: msg.username.clone(),
            name_color: if config.disable_custom_colors {
                String::new()
            } else {
                msg.name_color.clone()
            },
            bracket_content: bracket_content(msg, config),
            content: msg.content.clone(),
            image_art: msg.image_art.clone(),
            image_status: msg.image_status.clone(),
            dimmed,
        };
        let rows_needed = rows_needed(&line, cols_usize);
        if used_rows + rows_needed > max_chat_rows {
            if selected.is_empty() && max_chat_rows > 1 {
                // Even the newest message doesn't fit — truncate its content.
                let mut truncated = line;
                let avail = max_chat_rows.saturating_sub(1).max(1);
                let max_chars = cols_usize.saturating_mul(avail).max(1);
                truncated.content = truncated
                    .content
                    .chars()
                    .take(max_chars.saturating_sub(1))
                    .collect::<String>();
                truncated.content.push('…');
                selected.push(truncated);
            }
            break;
        }
        used_rows += rows_needed;
        selected.push(line);
    }
    selected.reverse();

    let default_color = "\x1b[35m";
    let dim_color = "\x1b[90m";
    for line in &selected {
        let name_prefix = if line.dimmed {
            dim_color.to_string()
        } else if !line.name_color.is_empty() && line.name_color.len() >= 7 {
            format!(
                "\x1b[38;2;{};{};{}m",
                u8::from_str_radix(&line.name_color[1..3], 16).unwrap_or(255),
                u8::from_str_radix(&line.name_color[3..5], 16).unwrap_or(255),
                u8::from_str_radix(&line.name_color[5..7], 16).unwrap_or(255),
            )
        } else {
            default_color.to_string()
        };

        let bracket = if !line.username.is_empty() {
            line.bracket_content
                .replacen(&line.username, &format!("{}{}\x1b[0m", name_prefix, line.username), 1)
        } else {
            line.bracket_content.clone()
        };

        let bracket_color = if line.dimmed { dim_color } else { default_color };
        print!(
            "{}[{}]:\x1b[0m\r\n",
            if bracket.is_empty() { "" } else { bracket_color },
            bracket
        );

        if let Some(art) = &line.image_art {
            for art_line in art.lines() {
                if line.dimmed {
                    print!("{}{}\x1b[0m\r\n", dim_color, art_line);
                } else {
                    print!("{}\r\n", art_line);
                }
            }
        } else if line.image_status.is_some() {
            // Image was registered but couldn't be embedded — plain placeholder.
            if line.dimmed {
                print!("{}\x1b[2m<image>\x1b[0m\r\n", dim_color);
            } else {
                print!("\x1b[2m<image>\x1b[0m\r\n");
            }
        }

        print!(
            "{}{}\x1b[0m\r\n\r\n",
            if line.dimmed { dim_color } else { "\x1b[37m" },
            line.content
        );
    }

    // 3. Status bar
    let dot_color = if status.connected { "\x1b[32m" } else { "\x1b[31m" };
    let login_bit = match login {
        Some(l) if !l.handle.is_empty() => {
            format!(" | {} {} ({})", l.platform, l.handle, l.effective.label())
        }
        Some(l) if !l.platform.is_empty() => format!(" | logged in ({})", l.platform),
        _ => " | not logged in (l to login)".to_string(),
    };
    let scroll_bit = if ui.scroll > 0 {
        format!(" | ↑frozen ({} msgs)", ui.scroll)
    } else {
        String::new()
    };
    let rest_text = format!(
        " {} | {} messages buffered | {}x{}{}{} ",
        status.detail,
        messages.len(),
        cols,
        rows,
        login_bit,
        scroll_bit
    );

    let avail_for_rest = cols_usize.saturating_sub(2);
    let mut rest_chars: Vec<char> = rest_text.chars().collect();
    if rest_chars.len() > avail_for_rest {
        rest_chars.truncate(avail_for_rest);
    } else {
        rest_chars.resize(avail_for_rest, ' ');
    }
    let rest_str: String = rest_chars.into_iter().collect();
    stdout.execute(cursor::MoveTo(0, rows.saturating_sub(2) as u16))?;
    print!("\x1b[100m\x1b[37m {}●\x1b[37m{}\x1b[0m", dot_color, rest_str);

    // 4. Footer
    stdout.execute(cursor::MoveTo(0, rows.saturating_sub(1) as u16))?;
    let footer = if ui.mode == UiMode::Input {
        " input: type + enter to send | tab: platform | esc: cancel "
    } else if login.is_some() && login.map(|l| l.can_moderate()).unwrap_or(false) {
        " q: exit | l: login | i: send | up/down/wheel: scroll | enter: mod menu | esc: back "
    } else {
        " q: exit | l: login | up/down/wheel: scroll "
    };
    let padded_footer = format!("{:^width$}", footer, width = cols_usize);
    print!("\x1b[90m{}\x1b[0m", padded_footer);

    // 5. Overlays
    draw_overlays(stdout, cols, rows, ui, login)?;

    stdout.flush()?;
    Ok(())
}

fn draw_box(
    stdout: &mut io::Stdout,
    cols: usize,
    rows: usize,
    lines: &[String],
) -> io::Result<()> {
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(10) + 4;
    let width = width.min(cols.saturating_sub(2).max(10));
    let height = lines.len() + 2;
    let start_x = (cols.saturating_sub(width)) / 2;
    let start_y = rows.saturating_sub(height) / 2;

    for (i, line) in lines.iter().enumerate() {
        stdout.execute(cursor::MoveTo(start_x as u16, (start_y + i) as u16))?;
        let pad = width.saturating_sub(4);
        let mut chars: Vec<char> = line.chars().collect();
        if chars.len() > pad {
            chars.truncate(pad.saturating_sub(1));
            chars.push('…');
        }
        let content: String = chars.into_iter().collect();
        print!(
            "\x1b[100m\x1b[37m│ {:<pad$} │\x1b[0m",
            content,
            pad = pad
        );
    }
    Ok(())
}

fn draw_overlays(
    stdout: &mut io::Stdout,
    cols: usize,
    rows: usize,
    ui: &Ui,
    login: Option<&LoginState>,
) -> io::Result<()> {
    match ui.mode {
        UiMode::Chat => {
            if !ui.result_note.is_empty() {
                let lines = vec![ui.result_note.clone()];
                draw_box(stdout, cols, rows, &lines)?;
            }
            Ok(())
        }
        UiMode::LoginMenu => {
            let mut lines = vec![
                "─ LOGIN ─".to_string(),
                " 1: Twitch".to_string(),
                " 2: Kick (enter handle after auth)".to_string(),
                " 3: YouTube".to_string(),
                " 4: cancel".to_string(),
            ];
            if !ui.login_note.is_empty() {
                lines.push(format!(" ⚠ {}", ui.login_note));
            }
            if let Some(l) = login {
                if !l.handle.is_empty() {
                    lines.push(format!(
                        " logged in as {} {} ({})",
                        l.platform,
                        l.handle,
                        l.effective.label()
                    ));
                }
            }
            draw_box(stdout, cols, rows, &lines)
        }
        UiMode::KickHandle => {
            let mut lines = vec![" Enter your Kick username:".to_string()];
            lines.push(format!(" > {}", ui.kick_handle_draft));
            if !ui.login_note.is_empty() {
                lines.push(format!(" ⚠ {}", ui.login_note));
            }
            draw_box(stdout, cols, rows, &lines)
        }
        UiMode::ActionMenu => {
            let target = ui
                .action_target
                .as_ref()
                .map(|(u, h, p, _)| format!("{} ({})", if u.is_empty() { h.as_str() } else { u.as_str() }, p))
                .unwrap_or_default();
            let mut lines = vec![format!("─ MOD ACTIONS: {} ─", target)];
            lines.push(" 1: ban".to_string());
            lines.push(" 2: timeout".to_string());
            lines.push(" 3: commend (+1 score)".to_string());
            lines.push(" 4: reprimand (-1 score)".to_string());
            lines.push(" esc: back".to_string());
            if !ui.result_note.is_empty() {
                lines.push(format!(" ⚠ {}", ui.result_note));
            }
            draw_box(stdout, cols, rows, &lines)
        }
        UiMode::TimeoutPrompt => {
            let lines = vec![
                " Timeout duration in seconds (default 300):".to_string(),
                format!(" > {}", ui.timeout_draft),
            ];
            draw_box(stdout, cols, rows, &lines)
        }
        UiMode::Input => {
            // Input bar rendered at the bottom of the message area.
            stdout.execute(cursor::MoveTo(0, rows.saturating_sub(3) as u16))?;
            let prefix = format!("[{}] > ", ui.target_platform);
            let max_w = cols.saturating_sub(prefix.chars().count().max(1)).max(1);
            let mut chars: Vec<char> = ui.draft.chars().collect();
            if chars.len() > max_w {
                chars.truncate(max_w.saturating_sub(1));
                chars.push('…');
            }
            let draft: String = chars.into_iter().collect();
            let padded = format!("{:width$}", draft, width = max_w);
            print!("\x1b[40m\x1b[37m{}{}\x1b[0m", prefix, padded);
            Ok(())
        }
    }
}