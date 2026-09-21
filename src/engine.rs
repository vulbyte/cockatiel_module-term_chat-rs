use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};

use cockatiel_client::proto::container::Payload;
use cockatiel_client::proto::*;
use futures_util::SinkExt;
use prost::Message as ProstMessage;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use crate::login::Roles;

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    WsMessage,
>;

/// Handle to the connected engine. Owns the write half of the WebSocket and a
/// broadcast channel carrying `DatabaseQueryResult`s (fed by the read task).
#[derive(Clone)]
pub struct EngineHandle {
    pub auth_token: String,
    pub module_name: String,
    pub instance_uuid7: String,
    write: Arc<Mutex<WsSink>>,
    results: broadcast::Sender<DatabaseQueryResult>,
}

pub const CHAT_VERIFY_QUERY: &str = "chat_verify_identity";

impl EngineHandle {
    pub fn new(
        auth_token: String,
        module_name: String,
        instance_uuid7: String,
        write: WsSink,
    ) -> Self {
        let (results, _) = broadcast::channel(256);
        Self {
            auth_token,
            module_name,
            instance_uuid7,
            write: Arc::new(Mutex::new(write)),
            results,
        }
    }

    pub fn result_sender(&self) -> broadcast::Sender<DatabaseQueryResult> {
        self.results.clone()
    }

    pub async fn send_payload(&self, payload: Payload) -> Result<(), String> {
        let container = Container {
            version: 1,
            auth_token: self.auth_token.clone(),
            module_name: self.module_name.clone(),
            module_instance_uuid7: self.instance_uuid7.clone(),
            payload: Some(payload),
        };
        let mut buf = Vec::new();
        container
            .encode(&mut buf)
            .map_err(|e| format!("encode error: {}", e))?;
        let mut write = self.write.lock().await;
        write
            .send(WsMessage::Binary(buf))
            .await
            .map_err(|e| format!("send error: {}", e))
    }

    /// Send a DatabaseQuery and wait for its matching DatabaseQueryResult.
    pub async fn db_query(&self, query_id: &str, sql: &str) -> Result<DatabaseQueryResult, String> {
        let mut rx = self.results.subscribe();
        self.send_payload(Payload::DatabaseQuery(DatabaseQuery {
            query_id: query_id.to_string(),
            sql: sql.to_string(),
            params: vec![],
        }))
        .await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out waiting for query response '{}'", query_id));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(res)) => {
                    if res.query_id == query_id {
                        return Ok(res);
                    }
                }
                Ok(Err(_)) => return Err("query response channel closed".to_string()),
                Err(_) => return Err(format!("timed out waiting for query response '{}'", query_id)),
            }
        }
    }

    /// Answer a prompt (e.g. an audit review) broadcast by the engine. `reason`
/// carries free-text input for `input_label` prompts.
    pub async fn send_prompt_response(
        &self,
        prompt_id: &str,
        accepted: bool,
        reason: &str,
    ) -> Result<(), String> {
        self.send_payload(Payload::PromptResponse(PromptResponse {
            prompt_id_uuid7: prompt_id.to_string(),
            accepted,
            reason: reason.to_string(),
        }))
        .await
    }

    /// Acknowledge receipt of a pipeline message so the engine can advance the
    /// message chain without waiting for the ack timeout.
    pub async fn ack_message(&self, message_uuid7: &str) -> Result<(), String> {
        if message_uuid7.is_empty() {
            return Ok(());
        }
        self.send_payload(Payload::MessageAck(MessageAck {
            message_uuid7: message_uuid7.to_string(),
        }))
        .await
    }

    /// Ask the engine to log who we are + our platform-verified roles into the
    /// user database (write-through). Returns the effective roles.
    pub async fn verify_identity(
        &self,
        platform: &str,
        handle: &str,
        verified: &Roles,
    ) -> Result<Roles, String> {
        let payload = serde_json::json!({
            "platform": platform,
            "handle": handle,
            "verified_roles": {
                "is_sponsor": verified.is_sponsor,
                "is_moderator": verified.is_moderator,
                "is_admin": verified.is_admin,
                "is_owner": verified.is_owner,
            },
        });
        let resp = self
            .db_query(CHAT_VERIFY_QUERY, &payload.to_string())
            .await?;
        if !resp.success {
            return Err(resp.error);
        }
        let json: serde_json::Value = serde_json::from_slice(&resp.result_blob)
            .map_err(|e| format!("bad verify_identity response: {}", e))?;
        let user = json.get("user").cloned().unwrap_or(serde_json::json!({}));
        Ok(Roles {
            is_sponsor: user.get("is_sponsor").and_then(|v| v.as_bool()).unwrap_or(false),
            is_moderator: user.get("is_moderator").and_then(|v| v.as_bool()).unwrap_or(false),
            is_admin: user.get("is_admin").and_then(|v| v.as_bool()).unwrap_or(false),
            is_owner: user.get("is_owner").and_then(|v| v.as_bool()).unwrap_or(false),
        })
    }

    /// Send a chat message to a platform through the engine (appears as cockatiel).
    pub async fn send_message(
        &self,
        platform: &str,
        msg: &str,
        actor: Option<(&str, &str)>,
    ) -> Result<(), String> {
        let (actor_platform, actor_handle) = actor.unwrap_or(("", ""));
        self.send_payload(Payload::SendToPlatforms(SendToPlatforms {
            msg: msg.to_string(),
            level: PlatformSendLevel::All as i32,
            module_uuid7: String::new(),
            pid: String::new(),
            platform: platform.to_string(),
            actor_platform: actor_platform.to_string(),
            actor_handle: actor_handle.to_string(),
            actor_uuid7: String::new(),
        }))
        .await
    }

    /// Execute a mod action (`mod_ban`, `mod_timeout`, `mod_commend`,
    /// `mod_reprimand`) with the target payload JSON plus the actor identity.
    pub async fn mod_action(
        &self,
        query_id: &str,
        target: &serde_json::Value,
        actor: &crate::login::LoginState,
    ) -> Result<DatabaseQueryResult, String> {
        let mut payload = target.clone();
        payload["actor"] = serde_json::json!({
            "platform": actor.platform,
            "handle": actor.handle,
        });
        self.db_query(query_id, &payload.to_string()).await
    }

    /// Fetch the module list from the engine (open to any module) to read
    /// adapter credential values (client ids/secrets for OAuth flows).
    pub async fn module_list(&self) -> Result<Vec<serde_json::Value>, String> {
        let resp = self.db_query("module_list", "").await?;
        if !resp.success {
            return Err(resp.error);
        }
        serde_json::from_slice(&resp.result_blob).map_err(|e| format!("bad module_list: {}", e))
    }

    /// Read one adapter's credential_values from the module list.
    pub async fn adapter_credentials(
        &self,
        adapter_name: &str,
    ) -> Result<std::collections::HashMap<String, String>, String> {
        let list = self.module_list().await?;
        for entry in &list {
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name != adapter_name {
                continue;
            }
            let mut map = std::collections::HashMap::new();
            if let Some(vals) = entry.get("credential_values").and_then(|v| v.as_object()) {
                for (k, v) in vals {
                    if let Some(s) = v.as_str() {
                        map.insert(k.clone(), s.to_string());
                    }
                }
            }
            return Ok(map);
        }
        Err(format!("adapter '{}' not found in module list", adapter_name))
    }
}