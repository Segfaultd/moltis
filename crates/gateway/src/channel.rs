use std::sync::Arc;

use {
    async_trait::async_trait,
    serde_json::Value,
    tokio::sync::RwLock,
    tracing::{error, info, warn},
};

#[cfg(feature = "slack")]
use moltis_slack::SlackPlugin;
use {moltis_channels::ChannelPlugin, moltis_telegram::TelegramPlugin};

use {
    moltis_channels::{
        message_log::MessageLog,
        store::{ChannelStore, StoredChannel},
    },
    moltis_sessions::metadata::SqliteSessionMetadata,
};

use crate::services::{ChannelService, ServiceResult};

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub struct LiveChannelService {
    telegram: Arc<RwLock<TelegramPlugin>>,
    #[cfg(feature = "slack")]
    slack: Arc<RwLock<SlackPlugin>>,
    store: Arc<dyn ChannelStore>,
    message_log: Arc<dyn MessageLog>,
    session_metadata: Arc<SqliteSessionMetadata>,
}

impl LiveChannelService {
    pub fn new(
        telegram: TelegramPlugin,
        #[cfg(feature = "slack")] slack: SlackPlugin,
        store: Arc<dyn ChannelStore>,
        message_log: Arc<dyn MessageLog>,
        session_metadata: Arc<SqliteSessionMetadata>,
    ) -> Self {
        Self {
            telegram: Arc::new(RwLock::new(telegram)),
            #[cfg(feature = "slack")]
            slack: Arc::new(RwLock::new(slack)),
            store,
            message_log,
            session_metadata,
        }
    }

    fn status_entry(
        channel_type: &str,
        aid: &str,
        connected: bool,
        details: Option<String>,
        cfg: Option<Value>,
        sessions: Vec<Value>,
    ) -> Value {
        let label = if channel_type == "slack" {
            "Slack"
        } else {
            "Telegram"
        };
        let mut entry = serde_json::json!({
            "type": channel_type,
            "name": format!("{label} ({aid})"),
            "account_id": aid,
            "status": if connected { "connected" } else { "disconnected" },
            "details": details,
        });
        if let Some(config) = cfg {
            entry["config"] = config;
        }
        if !sessions.is_empty() {
            entry["sessions"] = serde_json::json!(sessions);
        }
        entry
    }

    async fn build_sessions(&self, channel_type: &str, aid: &str) -> Vec<Value> {
        let bound = self
            .session_metadata
            .list_account_sessions(channel_type, aid)
            .await;
        let active_map = self
            .session_metadata
            .list_active_sessions(channel_type, aid)
            .await;
        bound
            .iter()
            .map(|s| {
                let is_active = active_map.iter().any(|(_, sk)| sk == &s.key);
                serde_json::json!({
                    "key": s.key,
                    "label": s.label,
                    "messageCount": s.message_count,
                    "active": is_active,
                })
            })
            .collect()
    }

    async fn resolve_channel_type(&self, account_id: &str, input: Option<&str>) -> Option<String> {
        if let Some(t) = input {
            return Some(t.to_string());
        }
        if let Some(stored) = self.store.get(account_id).await.ok().flatten() {
            return Some(stored.channel_type);
        }
        if self
            .telegram
            .read()
            .await
            .account_ids()
            .iter()
            .any(|id| id == account_id)
        {
            return Some("telegram".to_string());
        }
        #[cfg(feature = "slack")]
        if self
            .slack
            .read()
            .await
            .account_ids()
            .iter()
            .any(|id| id == account_id)
        {
            return Some("slack".to_string());
        }
        None
    }
}

#[async_trait]
impl ChannelService for LiveChannelService {
    async fn status(&self) -> ServiceResult {
        let mut channels = Vec::new();

        {
            let tg = self.telegram.read().await;
            let account_ids = tg.account_ids();
            if let Some(status) = tg.status() {
                for aid in &account_ids {
                    match status.probe(aid).await {
                        Ok(snap) => {
                            let sessions = self.build_sessions("telegram", aid).await;
                            channels.push(Self::status_entry(
                                "telegram",
                                aid,
                                snap.connected,
                                snap.details,
                                tg.account_config(aid),
                                sessions,
                            ));
                        },
                        Err(e) => {
                            channels.push(serde_json::json!({
                                "type": "telegram",
                                "name": format!("Telegram ({})", aid),
                                "account_id": aid,
                                "status": "error",
                                "details": e.to_string(),
                            }));
                        },
                    }
                }
            }
        }

        #[cfg(feature = "slack")]
        {
            let sl = self.slack.read().await;
            let account_ids = sl.account_ids();
            if let Some(status) = sl.status() {
                for aid in &account_ids {
                    match status.probe(aid).await {
                        Ok(snap) => {
                            let sessions = self.build_sessions("slack", aid).await;
                            channels.push(Self::status_entry(
                                "slack",
                                aid,
                                snap.connected,
                                snap.details,
                                sl.account_config(aid),
                                sessions,
                            ));
                        },
                        Err(e) => {
                            channels.push(serde_json::json!({
                                "type": "slack",
                                "name": format!("Slack ({})", aid),
                                "account_id": aid,
                                "status": "error",
                                "details": e.to_string(),
                            }));
                        },
                    }
                }
            }
        }

        Ok(serde_json::json!({ "channels": channels }))
    }

    async fn add(&self, params: Value) -> ServiceResult {
        let channel_type = params
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("telegram");

        let account_id = params
            .get("account_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'account_id'".to_string())?;

        let config = params
            .get("config")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));

        match channel_type {
            "telegram" => {
                info!(account_id, "adding telegram channel account");
                let mut tg = self.telegram.write().await;
                tg.start_account(account_id, config.clone())
                    .await
                    .map_err(|e| {
                        error!(error = %e, account_id, "failed to start telegram account");
                        e.to_string()
                    })?;
            },
            #[cfg(feature = "slack")]
            "slack" => {
                info!(account_id, "adding slack channel account");
                let mut sl = self.slack.write().await;
                sl.start_account(account_id, config.clone())
                    .await
                    .map_err(|e| {
                        error!(error = %e, account_id, "failed to start slack account");
                        e.to_string()
                    })?;
            },
            _ => return Err(format!("unsupported channel type: {channel_type}")),
        }

        let now = unix_now();
        if let Err(e) = self
            .store
            .upsert(StoredChannel {
                account_id: account_id.to_string(),
                channel_type: channel_type.into(),
                config,
                created_at: now,
                updated_at: now,
            })
            .await
        {
            warn!(error = %e, account_id, "failed to persist channel");
        }

        Ok(serde_json::json!({ "added": account_id }))
    }

    async fn remove(&self, params: Value) -> ServiceResult {
        let account_id = params
            .get("account_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'account_id'".to_string())?;
        let channel_type = self
            .resolve_channel_type(account_id, params.get("type").and_then(|v| v.as_str()))
            .await
            .unwrap_or_else(|| "telegram".to_string());

        match channel_type.as_str() {
            "telegram" => {
                info!(account_id, "removing telegram channel account");
                let mut tg = self.telegram.write().await;
                tg.stop_account(account_id).await.map_err(|e| {
                    error!(error = %e, account_id, "failed to stop telegram account");
                    e.to_string()
                })?;
            },
            #[cfg(feature = "slack")]
            "slack" => {
                info!(account_id, "removing slack channel account");
                let mut sl = self.slack.write().await;
                sl.stop_account(account_id).await.map_err(|e| {
                    error!(error = %e, account_id, "failed to stop slack account");
                    e.to_string()
                })?;
            },
            _ => return Err(format!("unsupported channel type: {channel_type}")),
        }

        if let Err(e) = self.store.delete(account_id).await {
            warn!(error = %e, account_id, "failed to delete channel from store");
        }

        Ok(serde_json::json!({ "removed": account_id }))
    }

    async fn logout(&self, params: Value) -> ServiceResult {
        self.remove(params).await
    }

    async fn update(&self, params: Value) -> ServiceResult {
        let account_id = params
            .get("account_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'account_id'".to_string())?;
        let channel_type = self
            .resolve_channel_type(account_id, params.get("type").and_then(|v| v.as_str()))
            .await
            .unwrap_or_else(|| "telegram".to_string());

        let config = params
            .get("config")
            .cloned()
            .ok_or_else(|| "missing 'config'".to_string())?;

        match channel_type.as_str() {
            "telegram" => {
                let mut tg = self.telegram.write().await;
                tg.stop_account(account_id)
                    .await
                    .map_err(|e| e.to_string())?;
                tg.start_account(account_id, config.clone())
                    .await
                    .map_err(|e| e.to_string())?;
            },
            #[cfg(feature = "slack")]
            "slack" => {
                let mut sl = self.slack.write().await;
                sl.stop_account(account_id)
                    .await
                    .map_err(|e| e.to_string())?;
                sl.start_account(account_id, config.clone())
                    .await
                    .map_err(|e| e.to_string())?;
            },
            _ => return Err(format!("unsupported channel type: {channel_type}")),
        }

        let stored = self
            .store
            .get(account_id)
            .await
            .map_err(|e| e.to_string())?;
        let created_at = stored.map_or_else(unix_now, |s| s.created_at);
        let now = unix_now();
        if let Err(e) = self
            .store
            .upsert(StoredChannel {
                account_id: account_id.to_string(),
                channel_type,
                config,
                created_at,
                updated_at: now,
            })
            .await
        {
            warn!(error = %e, account_id, "failed to persist channel update");
        }

        Ok(serde_json::json!({ "updated": account_id }))
    }

    async fn send(&self, _params: Value) -> ServiceResult {
        Err("direct channel send not yet implemented".into())
    }

    async fn senders_list(&self, params: Value) -> ServiceResult {
        let account_id = params
            .get("account_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'account_id'".to_string())?;
        let channel_type = self
            .resolve_channel_type(account_id, params.get("type").and_then(|v| v.as_str()))
            .await
            .unwrap_or_else(|| "telegram".to_string());

        let senders = self
            .message_log
            .unique_senders(account_id)
            .await
            .map_err(|e| e.to_string())?;

        let (allowlist, otp_challenges) = match channel_type.as_str() {
            "telegram" => {
                let tg = self.telegram.read().await;
                let allowlist: Vec<String> = tg
                    .account_config(account_id)
                    .and_then(|cfg| cfg.get("allowlist").cloned())
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                (allowlist, tg.pending_otp_challenges(account_id))
            },
            #[cfg(feature = "slack")]
            "slack" => {
                let sl = self.slack.read().await;
                let allowlist: Vec<String> = sl
                    .account_config(account_id)
                    .and_then(|cfg| cfg.get("allowlist").cloned())
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                (allowlist, sl.pending_otp_challenges(account_id))
            },
            _ => (Vec::new(), Vec::new()),
        };

        let list: Vec<Value> = senders
            .into_iter()
            .map(|s| {
                let is_allowed = allowlist.iter().any(|a| {
                    let a_lower = a.to_lowercase();
                    a_lower == s.peer_id.to_lowercase()
                        || s.username
                            .as_ref()
                            .is_some_and(|u| a_lower == u.to_lowercase())
                });
                let mut entry = serde_json::json!({
                    "peer_id": s.peer_id,
                    "username": s.username,
                    "sender_name": s.sender_name,
                    "message_count": s.message_count,
                    "last_seen": s.last_seen,
                    "allowed": is_allowed,
                });
                if let Some(otp) = otp_challenges.iter().find(|c| c.peer_id == s.peer_id) {
                    entry["otp_pending"] = serde_json::json!({
                        "code": otp.code,
                        "expires_at": otp.expires_at,
                    });
                }
                entry
            })
            .collect();

        Ok(serde_json::json!({ "senders": list }))
    }

    async fn sender_approve(&self, params: Value) -> ServiceResult {
        let account_id = params
            .get("account_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'account_id'".to_string())?;
        let channel_type = self
            .resolve_channel_type(account_id, params.get("type").and_then(|v| v.as_str()))
            .await
            .unwrap_or_else(|| "telegram".to_string());
        let identifier = params
            .get("identifier")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'identifier'".to_string())?;

        let stored = self
            .store
            .get(account_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("channel '{account_id}' not found in store"))?;

        let mut config = stored.config.clone();
        let allowlist = config
            .as_object_mut()
            .ok_or_else(|| "config is not an object".to_string())?
            .entry("allowlist")
            .or_insert_with(|| serde_json::json!([]));

        let arr = allowlist
            .as_array_mut()
            .ok_or_else(|| "allowlist is not an array".to_string())?;

        let id_lower = identifier.to_lowercase();
        if !arr
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.to_lowercase() == id_lower))
        {
            arr.push(serde_json::json!(identifier));
        }

        if let Some(obj) = config.as_object_mut() {
            obj.insert("dm_policy".into(), serde_json::json!("allowlist"));
        }

        let now = unix_now();
        if let Err(e) = self
            .store
            .upsert(StoredChannel {
                account_id: account_id.to_string(),
                channel_type: channel_type.clone(),
                config: config.clone(),
                created_at: stored.created_at,
                updated_at: now,
            })
            .await
        {
            warn!(error = %e, account_id, "failed to persist sender approval");
        }

        match channel_type.as_str() {
            "telegram" => {
                let tg = self.telegram.read().await;
                if let Err(e) = tg.update_account_config(account_id, config) {
                    warn!(error = %e, account_id, "failed to hot-update telegram config");
                }
            },
            #[cfg(feature = "slack")]
            "slack" => {
                let sl = self.slack.read().await;
                if let Err(e) = sl.update_account_config(account_id, config) {
                    warn!(error = %e, account_id, "failed to hot-update slack config");
                }
            },
            _ => {},
        }

        Ok(serde_json::json!({ "approved": identifier }))
    }

    async fn sender_deny(&self, params: Value) -> ServiceResult {
        let account_id = params
            .get("account_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'account_id'".to_string())?;
        let channel_type = self
            .resolve_channel_type(account_id, params.get("type").and_then(|v| v.as_str()))
            .await
            .unwrap_or_else(|| "telegram".to_string());
        let identifier = params
            .get("identifier")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'identifier'".to_string())?;

        let stored = self
            .store
            .get(account_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("channel '{account_id}' not found in store"))?;

        let mut config = stored.config.clone();
        if let Some(arr) = config
            .as_object_mut()
            .and_then(|o| o.get_mut("allowlist"))
            .and_then(|v| v.as_array_mut())
        {
            let id_lower = identifier.to_lowercase();
            arr.retain(|v| v.as_str().is_none_or(|s| s.to_lowercase() != id_lower));
        }

        let now = unix_now();
        if let Err(e) = self
            .store
            .upsert(StoredChannel {
                account_id: account_id.to_string(),
                channel_type: channel_type.clone(),
                config: config.clone(),
                created_at: stored.created_at,
                updated_at: now,
            })
            .await
        {
            warn!(error = %e, account_id, "failed to persist sender denial");
        }

        match channel_type.as_str() {
            "telegram" => {
                let tg = self.telegram.read().await;
                if let Err(e) = tg.update_account_config(account_id, config) {
                    warn!(error = %e, account_id, "failed to hot-update telegram config");
                }
            },
            #[cfg(feature = "slack")]
            "slack" => {
                let sl = self.slack.read().await;
                if let Err(e) = sl.update_account_config(account_id, config) {
                    warn!(error = %e, account_id, "failed to hot-update slack config");
                }
            },
            _ => {},
        }

        Ok(serde_json::json!({ "denied": identifier }))
    }
}
