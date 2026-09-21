use serde::{Deserialize, Serialize};
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use crate::config::ChatConfig;
use crate::engine::EngineHandle;

const REDIRECT_URI: &str = "http://localhost:3000";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Roles {
    pub is_sponsor: bool,
    pub is_moderator: bool,
    pub is_admin: bool,
    pub is_owner: bool,
}

impl Roles {
    pub fn can_moderate(&self) -> bool {
        self.is_moderator || self.is_admin || self.is_owner
    }
    pub fn label(&self) -> &'static str {
        if self.is_owner {
            "OWNER"
        } else if self.is_admin {
            "ADMIN"
        } else if self.is_moderator {
            "MOD"
        } else if self.is_sponsor {
            "SUB"
        } else {
            "user"
        }
    }
}

/// Persisted login state for the operator of this chat app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginState {
    pub platform: String,
    pub handle: String,
    pub token: String,
    pub send_scope: bool,
    pub verified: Roles,
    pub effective: Roles,
}

impl LoginState {
    pub fn can_send(&self) -> bool {
        self.send_scope || self.effective.can_moderate()
    }
    pub fn can_moderate(&self) -> bool {
        self.effective.can_moderate()
    }
}

fn load_login() -> Option<LoginState> {
    std::fs::read_to_string("login.json")
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
}

pub fn save_login(state: &LoginState) {
    if let Ok(pretty) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write("login.json", pretty);
    }
}

pub fn load_or_empty() -> LoginState {
    load_login().unwrap_or_else(|| LoginState {
        platform: String::new(),
        handle: String::new(),
        token: String::new(),
        send_scope: false,
        verified: Roles::default(),
        effective: Roles::default(),
    })
}

// ── OAuth redirect capture (localhost:3000) ────────────────────────────

async fn send_html(socket: &mut tokio::net::TcpStream, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    socket.write_all(resp.as_bytes()).await
}

fn extract_value(request: &str, param: &str) -> Option<String> {
    let marker = format!("?{}", param);
    let pos = request.find(&marker)?;
    let rest = &request[pos + marker.len()..];
    let end = rest.find(' ').unwrap_or(rest.len());
    let mut val = rest[..end].to_string();
    if let Some(amp) = val.find('&') {
        val.truncate(amp);
    }
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Open the browser and wait for the OAuth redirect back to localhost:3000.
/// Handles both `?code=`/`?token=` query redirects and the `#access_token=`
/// fragment (via a JS forwarding page) used by Twitch's implicit flow.
async fn capture_auth_redirect(auth_url: &str) -> Result<String, String> {
    let listener = TcpListener::bind("127.0.0.1:3000")
        .await
        .map_err(|e| format!("could not bind localhost:3000: {}", e))?;
    let _ = open::that(auth_url);

    let ok_html = "<html><body style='background:#0e0e10;color:#efeff1;font-family:system-ui,sans-serif;text-align:center;padding-top:120px;'><h1 style='color:#a970ff;'>Authentication successful!</h1><p>You can close this window and return to your terminal.</p></body></html>";
    let error_html = "<html><body style='background:#0e0e10;color:#efeff1;font-family:system-ui,sans-serif;text-align:center;padding-top:120px;'><h1 style='color:#ff4f4f;'>Authentication failed</h1><p>Return to your terminal.</p></body></html>";
    let landing_html = "<html><script>if(window.location.hash){var h=new URLSearchParams(window.location.hash.substring(1));var t=h.get('access_token')||h.get('token');if(t){window.location.href='/callback?token='+encodeURIComponent(t);}}</script><body style='background:#0e0e10;color:#efeff1;font-family:system-ui,sans-serif;text-align:center;padding-top:120px;'><h1 style='color:#a970ff;'>Authenticating...</h1></body></html>";

    for _ in 0..10 {
        let (mut socket, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut buf = [0; 8192];
        let n = socket.read(&mut buf).await.map_err(|e| e.to_string())?;
        let request = String::from_utf8_lossy(&buf[..n]).to_string();

        if request.contains("error=") {
            let _ = send_html(&mut socket, error_html).await;
            return Err("OAuth redirect contained an error".to_string());
        }
        if let Some(val) = extract_value(&request, "token=") {
            let _ = send_html(&mut socket, ok_html).await;
            return Ok(val);
        }
        if let Some(val) = extract_value(&request, "code=") {
            let _ = send_html(&mut socket, ok_html).await;
            return Ok(val);
        }

        // Fragment-based flow: serve the JS forwarder, then wait for /callback?token=
        let _ = send_html(&mut socket, landing_html).await;
        let (mut socket2, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut buf2 = [0; 8192];
        let n2 = socket2.read(&mut buf2).await.map_err(|e| e.to_string())?;
        let request2 = String::from_utf8_lossy(&buf2[..n2]).to_string();
        if let Some(val) = extract_value(&request2, "token=") {
            let _ = send_html(&mut socket2, ok_html).await;
            return Ok(val);
        }
    }
    Err("timed out waiting for the OAuth redirect".to_string())
}

// ── PKCE helpers ───────────────────────────────────────────────────────

fn random_string(len: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut out = String::new();
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";
    while out.len() < len {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let idx = (seed >> 33) as usize % CHARS.len();
        out.push(CHARS[idx] as char);
    }
    out
}

fn pkce_pair() -> (String, String) {
    let verifier = random_string(64);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

// ── Twitch ─────────────────────────────────────────────────────────────

/// Twitch login: browser implicit-grant OAuth. Owner if the operator's user id
/// is the monitored channel's broadcaster id; mod if the channel appears in the
/// operator's moderated-channels list; send if the token has chat:edit.
pub async fn login_twitch(eng: &EngineHandle, channel: &str) -> Result<LoginState, String> {
    let creds = eng.adapter_credentials("twitch-adapter").await?;
    let client_id = creds.get("client_id").cloned().unwrap_or_default();
    if client_id.is_empty() {
        return Err("twitch-adapter has no client_id configured — set it via the TUI credential form".to_string());
    }

    let auth_url = format!(
        "https://id.twitch.tv/oauth2/authorize?client_id={}&redirect_uri={}&response_type=token&scope={}",
        client_id, REDIRECT_URI, "user:read+chat:read+chat:edit"
    );
    let raw = capture_auth_redirect(&auth_url).await?;
    let token = raw.trim_start_matches("oauth:").to_string();

    let client = reqwest::Client::new();
    let val: serde_json::Value = client
        .get("https://id.twitch.tv/oauth2/validate")
        .header("Authorization", format!("OAuth {}", token))
        .send()
        .await
        .map_err(|e| format!("twitch validate failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("twitch validate parse failed: {}", e))?;

    let login = val.get("login").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let user_id = val.get("user_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let scopes: Vec<String> = val
        .get("scopes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // Resolve the monitored channel -> broadcaster id.
    let mut broadcaster_id = String::new();
    if !channel.is_empty() {
        let users: serde_json::Value = client
            .get("https://api.twitch.tv/helix/users")
            .query(&[("login", channel)])
            .header("Authorization", format!("Bearer {}", token))
            .header("Client-Id", &client_id)
            .send()
            .await
            .map_err(|e| format!("twitch users failed: {}", e))?
            .json()
            .await
            .map_err(|e| format!("twitch users parse failed: {}", e))?;
        broadcaster_id = users
            .pointer("/data/0/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }

    let mut verified = Roles::default();
    if !broadcaster_id.is_empty() && user_id == broadcaster_id {
        verified.is_owner = true;
    }
    let modch: serde_json::Value = client
        .get("https://api.twitch.tv/helix/moderation/channels")
        .header("Authorization", format!("Bearer {}", token))
        .header("Client-Id", &client_id)
        .send()
        .await
        .map_err(|e| format!("twitch moderated channels failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("twitch moderated channels parse failed: {}", e))?;
    if let Some(data) = modch.get("data").and_then(|v| v.as_array()) {
        for ch in data {
            if ch.get("broadcaster_id").and_then(|v| v.as_str()) == Some(broadcaster_id.as_str()) {
                verified.is_moderator = true;
            }
        }
    }

    Ok(LoginState {
        platform: "twitch".to_string(),
        handle: login,
        token,
        send_scope: scopes.iter().any(|s| s == "chat:edit"),
        verified,
        effective: Roles::default(),
    })
}

// ── Kick ───────────────────────────────────────────────────────────────

/// Kick login: OAuth authorization-code + PKCE. Mod-level if the token has
/// moderation:ban; send if it has chat:write. Identity falls back to the
/// handle the operator entered (Kick has no reliable self-check endpoint).
pub async fn login_kick(eng: &EngineHandle, fallback_handle: &str) -> Result<LoginState, String> {
    let creds = eng.adapter_credentials("kick-adapter").await?;
    let client_id = creds.get("client_id").cloned().unwrap_or_default();
    let client_secret = creds.get("client_secret").cloned().unwrap_or_default();
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("kick-adapter needs client_id + client_secret — set them via the TUI credential form".to_string());
    }

    let (verifier, challenge) = pkce_pair();
    let auth_url = format!(
        "https://id.kick.com/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        client_id, REDIRECT_URI, "user:read+chat:write+moderation:ban", challenge, random_string(16)
    );
    let code = capture_auth_redirect(&auth_url).await?;

    let client = reqwest::Client::new();
    let tok: serde_json::Value = client
        .post("https://id.kick.com/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("kick token failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("kick token parse failed: {}", e))?;

    let access = tok
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("kick token response missing access_token: {:?}", tok))?
        .to_string();
    let scopes: Vec<String> = tok
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .split_whitespace()
        .map(String::from)
        .collect();

    let mut verified = Roles::default();
    if scopes.iter().any(|s| s == "moderation:ban") {
        verified.is_moderator = true;
    }
    let send_scope = scopes.iter().any(|s| s == "chat:write");

    // Try to derive the handle from the public API; fall back to operator entry.
    let mut handle = fallback_handle.to_string();
    if handle.is_empty() {
        if let Ok(resp) = client
            .get("https://api.kick.com/public/v1/users")
            .header("Authorization", format!("Bearer {}", access))
            .send()
            .await
        {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(u) = json.pointer("/data/0/username").and_then(|v| v.as_str()) {
                    handle = u.to_string();
                }
            }
        }
    }

    Ok(LoginState {
        platform: "kick".to_string(),
        handle,
        token: access,
        send_scope,
        verified,
        effective: Roles::default(),
    })
}

// ── YouTube ────────────────────────────────────────────────────────────

/// YouTube login: Google OAuth code flow. Owner if the operator's channel id
/// matches the monitored channel. Needs google_oauth_client_id/secret in
/// chat_config.json; mod/send perms fall back to the user database.
pub async fn login_youtube(eng: &EngineHandle, cfg: &ChatConfig) -> Result<LoginState, String> {
    if cfg.google_oauth_client_id.is_empty() || cfg.google_oauth_client_secret.is_empty() {
        return Err("YouTube login requires google_oauth_client_id + google_oauth_client_secret in chat_config.json".to_string());
    }
    let scope = "https://www.googleapis.com/auth/youtube.force-ssl";
    let auth_url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline",
        cfg.google_oauth_client_id, REDIRECT_URI, scope
    );
    let code = capture_auth_redirect(&auth_url).await?;

    let client = reqwest::Client::new();
    let tok: serde_json::Value = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code.as_str()),
            ("client_id", cfg.google_oauth_client_id.as_str()),
            ("client_secret", cfg.google_oauth_client_secret.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .map_err(|e| format!("google token failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("google token parse failed: {}", e))?;

    let access = tok
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "google token response missing access_token".to_string())?
        .to_string();

    let me: serde_json::Value = client
        .get("https://www.googleapis.com/youtube/v3/channels")
        .query(&[("part", "id,snippet"), ("mine", "true")])
        .header("Authorization", format!("Bearer {}", access))
        .send()
        .await
        .map_err(|e| format!("youtube channels failed: {}", e))?
        .json()
        .await
        .map_err(|e| format!("youtube channels parse failed: {}", e))?;

    let my_channel = me
        .pointer("/items/0/id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut handle = me
        .pointer("/items/0/snippet/customUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches('@').to_string())
        .unwrap_or_else(|| {
            me.pointer("/items/0/snippet/title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        });

    let mut verified = Roles::default();
    let creds = eng.adapter_credentials("youtube-adapter").await?;
    let monitored = creds.get("channel_id").cloned().unwrap_or_default();
    if !monitored.is_empty() && my_channel == monitored {
        verified.is_owner = true;
    }
    if handle.is_empty() {
        handle = my_channel;
    }

    Ok(LoginState {
        platform: "youtube".to_string(),
        handle,
        token: access,
        send_scope: false,
        verified,
        effective: Roles::default(),
    })
}

/// Perform a login for the given platform, then sync identity with the engine
/// (write-through) to get the effective roles.
pub async fn login_and_verify(
    eng: &EngineHandle,
    platform: &str,
    channel: &str,
    kick_handle: &str,
    cfg: &ChatConfig,
) -> Result<LoginState, String> {
    let mut state = match platform {
        "twitch" => login_twitch(eng, channel).await?,
        "kick" => login_kick(eng, kick_handle).await?,
        "youtube" => login_youtube(eng, cfg).await?,
        _ => return Err(format!("unknown platform '{}'", platform)),
    };
    if state.handle.is_empty() {
        return Err(format!(
            "could not determine your {} handle — log in again and enter your handle",
            platform
        ));
    }
    state.effective = eng.verify_identity(&state.platform, &state.handle, &state.verified).await?;
    save_login(&state);
    Ok(state)
}