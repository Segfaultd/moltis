use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use secrecy::ExposeSecret;
use tracing::{info, warn};

use moltis_channels::{
    ChannelEventSink,
    message_log::MessageLog,
    otp::OtpChallengeInfo,
    plugin::{ChannelHealthSnapshot, ChannelOutbound, ChannelPlugin, ChannelStatus},
};

use crate::{
    config::SlackAccountConfig,
    outbound::SlackOutbound,
    socket,
    state::{AccountState, AccountStateMap},
};

const PROBE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, serde::Deserialize)]
struct AuthTestResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    team: Option<String>,
}

pub struct SlackPlugin {
    accounts: AccountStateMap,
    outbound: SlackOutbound,
    message_log: Option<Arc<dyn MessageLog>>,
    event_sink: Option<Arc<dyn ChannelEventSink>>,
    probe_cache: RwLock<HashMap<String, (ChannelHealthSnapshot, Instant)>>,
}

impl SlackPlugin {
    #[must_use]
    pub fn new() -> Self {
        let accounts: AccountStateMap = Arc::new(RwLock::new(HashMap::new()));
        let outbound = SlackOutbound::new(Arc::clone(&accounts));
        Self {
            accounts,
            outbound,
            message_log: None,
            event_sink: None,
            probe_cache: RwLock::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_message_log(mut self, log: Arc<dyn MessageLog>) -> Self {
        self.message_log = Some(log);
        self
    }

    #[must_use]
    pub fn with_event_sink(mut self, sink: Arc<dyn ChannelEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    #[must_use]
    pub fn shared_outbound(&self) -> Arc<dyn ChannelOutbound> {
        Arc::new(SlackOutbound::new(Arc::clone(&self.accounts)))
    }

    #[must_use]
    pub fn account_ids(&self) -> Vec<String> {
        let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
        accounts.keys().cloned().collect()
    }

    pub fn account_config(&self, account_id: &str) -> Option<serde_json::Value> {
        let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
        accounts
            .get(account_id)
            .and_then(|s| serde_json::to_value(&s.config).ok())
    }

    pub fn update_account_config(&self, account_id: &str, config: serde_json::Value) -> Result<()> {
        let slack_config: SlackAccountConfig = serde_json::from_value(config)?;
        let mut accounts = self.accounts.write().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = accounts.get_mut(account_id) {
            state.config = slack_config;
            Ok(())
        } else {
            Err(anyhow!("account not found: {account_id}"))
        }
    }

    #[must_use]
    pub fn pending_otp_challenges(&self, account_id: &str) -> Vec<OtpChallengeInfo> {
        let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
        accounts
            .get(account_id)
            .map(|s| {
                let otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                otp.list_pending()
            })
            .unwrap_or_default()
    }
}

impl Default for SlackPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelPlugin for SlackPlugin {
    fn id(&self) -> &str {
        "slack"
    }

    fn name(&self) -> &str {
        "Slack"
    }

    async fn start_account(&mut self, account_id: &str, config: serde_json::Value) -> Result<()> {
        let cfg: SlackAccountConfig = serde_json::from_value(config)?;

        if cfg.app_token.expose_secret().is_empty() {
            return Err(anyhow!("slack app_token is required"));
        }
        if cfg.bot_token.expose_secret().is_empty() {
            return Err(anyhow!("slack bot_token is required"));
        }

        // Replace account if already running.
        if self
            .accounts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(account_id)
        {
            self.stop_account(account_id).await?;
        }

        let auth = slack_auth_test(cfg.bot_token.expose_secret()).await?;
        let bot_user_id = auth.user_id.clone();

        let cancel = tokio_util::sync::CancellationToken::new();
        let outbound = Arc::new(SlackOutbound::new(Arc::clone(&self.accounts)));
        let otp_cooldown = cfg.otp_cooldown_secs;
        let state = AccountState {
            account_id: account_id.to_string(),
            config: cfg,
            bot_user_id,
            outbound,
            cancel: cancel.clone(),
            message_log: self.message_log.clone(),
            event_sink: self.event_sink.clone(),
            otp: std::sync::Mutex::new(moltis_channels::otp::OtpState::new(otp_cooldown)),
        };

        {
            let mut accounts = self.accounts.write().unwrap_or_else(|e| e.into_inner());
            accounts.insert(account_id.to_string(), state);
        }

        let aid = account_id.to_string();
        let accounts = Arc::clone(&self.accounts);
        tokio::spawn(async move {
            if let Err(e) = socket::start_socket_mode(aid.clone(), accounts, cancel).await {
                warn!(account_id = aid, error = %e, "slack socket loop exited with error");
            }
        });

        info!(
            account_id,
            team = ?auth.team,
            user = ?auth.user,
            "slack account started"
        );

        Ok(())
    }

    async fn stop_account(&mut self, account_id: &str) -> Result<()> {
        let mut accounts = self.accounts.write().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = accounts.remove(account_id) {
            info!(account_id, "stopping slack account");
            state.cancel.cancel();
        }
        Ok(())
    }

    fn outbound(&self) -> Option<&dyn ChannelOutbound> {
        Some(&self.outbound)
    }

    fn status(&self) -> Option<&dyn ChannelStatus> {
        Some(self)
    }
}

#[async_trait]
impl ChannelStatus for SlackPlugin {
    async fn probe(&self, account_id: &str) -> Result<ChannelHealthSnapshot> {
        if let Ok(cache) = self.probe_cache.read()
            && let Some((snap, ts)) = cache.get(account_id)
            && ts.elapsed() < PROBE_CACHE_TTL
        {
            return Ok(snap.clone());
        }

        let token = {
            let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
            accounts
                .get(account_id)
                .map(|s| s.config.bot_token.expose_secret().clone())
        };

        let result = match token {
            Some(token) => match slack_auth_test(&token).await {
                Ok(auth) => ChannelHealthSnapshot {
                    connected: true,
                    account_id: account_id.to_string(),
                    details: Some(format!(
                        "Bot: {} ({})",
                        auth.user.unwrap_or_else(|| "unknown".to_string()),
                        auth.team.unwrap_or_else(|| "unknown team".to_string())
                    )),
                },
                Err(e) => ChannelHealthSnapshot {
                    connected: false,
                    account_id: account_id.to_string(),
                    details: Some(format!("API error: {e}")),
                },
            },
            None => ChannelHealthSnapshot {
                connected: false,
                account_id: account_id.to_string(),
                details: Some("account not started".into()),
            },
        };

        if let Ok(mut cache) = self.probe_cache.write() {
            cache.insert(account_id.to_string(), (result.clone(), Instant::now()));
        }

        Ok(result)
    }
}

async fn slack_auth_test(bot_token: &str) -> Result<AuthTestResponse> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://slack.com/api/auth.test")
        .bearer_auth(bot_token)
        .send()
        .await?;

    let parsed: AuthTestResponse = resp.json().await?;
    if !parsed.ok {
        return Err(anyhow!(
            "slack auth.test failed: {}",
            parsed.error.unwrap_or_else(|| "unknown error".to_string())
        ));
    }
    Ok(parsed)
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::Secret;

    #[test]
    fn plugin_id_and_name() {
        let p = SlackPlugin::new();
        assert_eq!(p.id(), "slack");
        assert_eq!(p.name(), "Slack");
    }

    #[test]
    fn config_update_preserves_otp_state() {
        let p = SlackPlugin::new();
        let cfg = SlackAccountConfig {
            app_token: Secret::new("xapp".into()),
            bot_token: Secret::new("xoxb".into()),
            ..Default::default()
        };
        let state = AccountState {
            account_id: "ws1".into(),
            config: cfg.clone(),
            bot_user_id: Some("U1".into()),
            outbound: Arc::new(SlackOutbound::new(Arc::clone(&p.accounts))),
            cancel: tokio_util::sync::CancellationToken::new(),
            message_log: None,
            event_sink: None,
            otp: std::sync::Mutex::new(moltis_channels::otp::OtpState::new(300)),
        };
        p.accounts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert("ws1".into(), state);
        let update = serde_json::json!({
            "app_token": "xapp2",
            "bot_token": "xoxb2",
            "dm_policy": "open",
            "channel_policy": "open",
            "mention_mode": "mention",
            "allowlist": [],
            "channel_allowlist": [],
            "stream_mode": "editinplace",
            "edit_throttle_ms": 1000,
            "otp_self_approval": true,
            "otp_cooldown_secs": 300,
            "chunk_size": 4000
        });
        let res = p.update_account_config("ws1", update);
        assert!(res.is_ok());
        let pending = p.pending_otp_challenges("ws1");
        assert!(pending.is_empty());
    }
}
