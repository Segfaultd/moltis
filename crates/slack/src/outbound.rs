use std::time::Instant;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use secrecy::ExposeSecret;

use moltis_channels::{ChannelOutbound, ChannelStreamOutbound, StreamEvent, StreamReceiver};
use moltis_common::types::ReplyPayload;

use crate::{
    markdown::{
        SLACK_DEFAULT_CHUNK_SIZE, SLACK_MAX_MESSAGE_LEN, chunk_message, markdown_to_mrkdwn,
    },
    state::AccountStateMap,
};

#[derive(Clone)]
pub struct SlackOutbound {
    pub(crate) accounts: AccountStateMap,
}

impl SlackOutbound {
    #[must_use]
    pub fn new(accounts: AccountStateMap) -> Self {
        Self { accounts }
    }

    fn account_snapshot(&self, account_id: &str) -> Result<(String, usize, u64)> {
        let accounts = self.accounts.read().unwrap_or_else(|e| e.into_inner());
        let state = accounts
            .get(account_id)
            .ok_or_else(|| anyhow!("unknown account: {account_id}"))?;
        Ok((
            state.config.bot_token.expose_secret().clone(),
            state.config.chunk_size,
            state.config.edit_throttle_ms,
        ))
    }

    async fn post_message(
        &self,
        token: &str,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<SlackPostMessageResponse> {
        let mut body = serde_json::json!({
            "channel": channel,
            "text": text,
            "mrkdwn": true,
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(ts) = thread_ts
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("thread_ts".to_string(), serde_json::json!(ts));
        }

        let client = reqwest::Client::new();
        let resp = client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;

        let parsed: SlackPostMessageResponse = resp.json().await?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack chat.postMessage failed: {}",
                parsed.error.unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        Ok(parsed)
    }

    async fn update_message(&self, token: &str, channel: &str, ts: &str, text: &str) -> Result<()> {
        let body = serde_json::json!({
            "channel": channel,
            "ts": ts,
            "text": text,
            "mrkdwn": true,
        });

        let client = reqwest::Client::new();
        let resp = client
            .post("https://slack.com/api/chat.update")
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;

        let parsed: SlackBasicResponse = resp.json().await?;
        if !parsed.ok {
            return Err(anyhow!(
                "slack chat.update failed: {}",
                parsed.error.unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        Ok(())
    }
}

#[derive(Debug, serde::Deserialize)]
struct SlackBasicResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct SlackPostMessageResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ts: Option<String>,
}

#[async_trait]
impl ChannelOutbound for SlackOutbound {
    async fn send_text(
        &self,
        account_id: &str,
        to: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let (token, chunk_size, _) = self.account_snapshot(account_id)?;
        let mrkdwn = markdown_to_mrkdwn(text);
        let chunks = chunk_message(&mrkdwn, chunk_size.max(SLACK_DEFAULT_CHUNK_SIZE));

        for chunk in chunks {
            self.post_message(&token, to, &chunk, reply_to).await?;
        }
        Ok(())
    }

    async fn send_media(
        &self,
        account_id: &str,
        to: &str,
        payload: &ReplyPayload,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let text = if let Some(media) = &payload.media {
            if payload.text.is_empty() {
                media.url.clone()
            } else {
                format!("{}\n{}", payload.text, media.url)
            }
        } else {
            payload.text.clone()
        };
        self.send_text(account_id, to, &text, reply_to).await
    }

    async fn send_typing(&self, account_id: &str, _to: &str) -> Result<()> {
        let _ = self.account_snapshot(account_id)?;
        Ok(())
    }

    async fn send_location(
        &self,
        account_id: &str,
        to: &str,
        latitude: f64,
        longitude: f64,
        title: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let text = match title {
            Some(place) => format!("{place}\nhttps://maps.google.com/?q={latitude},{longitude}"),
            None => format!("https://maps.google.com/?q={latitude},{longitude}"),
        };
        self.send_text(account_id, to, &text, reply_to).await
    }
}

#[async_trait]
impl ChannelStreamOutbound for SlackOutbound {
    async fn send_stream(
        &self,
        account_id: &str,
        to: &str,
        mut stream: StreamReceiver,
    ) -> Result<()> {
        let (token, _chunk_size, throttle_ms) = self.account_snapshot(account_id)?;

        // Post placeholder first then update in-place while deltas arrive.
        let placeholder = self
            .post_message(&token, to, "…", None)
            .await?
            .ts
            .ok_or_else(|| anyhow!("missing ts from slack postMessage response"))?;

        let mut full = String::new();
        let mut last_update = Instant::now();

        while let Some(evt) = stream.recv().await {
            match evt {
                StreamEvent::Delta(d) => {
                    full.push_str(&d);
                    if last_update.elapsed().as_millis() >= u128::from(throttle_ms) {
                        let mrkdwn = markdown_to_mrkdwn(&full);
                        let current = chunk_message(&mrkdwn, SLACK_MAX_MESSAGE_LEN)
                            .first()
                            .cloned()
                            .unwrap_or_default();
                        self.update_message(&token, to, &placeholder, &current)
                            .await?;
                        last_update = Instant::now();
                    }
                },
                StreamEvent::Done => break,
                StreamEvent::Error(err) => {
                    if full.is_empty() {
                        full = err;
                    }
                    break;
                },
            }
        }

        let mrkdwn = markdown_to_mrkdwn(&full);
        let mut chunked = chunk_message(&mrkdwn, SLACK_MAX_MESSAGE_LEN);
        let current = chunked.first().cloned().unwrap_or_default();
        self.update_message(&token, to, &placeholder, &current)
            .await?;
        if chunked.len() > 1 {
            for extra in chunked.drain(1..) {
                self.post_message(&token, to, &extra, Some(&placeholder))
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::Arc,
    };

    use super::*;
    use crate::{config::SlackAccountConfig, state::AccountState};
    use moltis_channels::{ChannelEventSink, message_log::MessageLog, otp::OtpState};
    use secrecy::Secret;
    use tokio_util::sync::CancellationToken;

    fn build_state_map() -> AccountStateMap {
        let map = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let outbound = Arc::new(SlackOutbound::new(Arc::clone(&map)));
        let state = AccountState {
            account_id: "test".into(),
            config: SlackAccountConfig {
                app_token: Secret::new("xapp-test".into()),
                bot_token: Secret::new("xoxb-test".into()),
                ..Default::default()
            },
            bot_user_id: Some("U1".into()),
            outbound,
            cancel: CancellationToken::new(),
            message_log: None::<Arc<dyn MessageLog>>,
            event_sink: None::<Arc<dyn ChannelEventSink>>,
            otp: std::sync::Mutex::new(OtpState::new(300)),
        };
        map.write()
            .unwrap_or_else(|e| e.into_inner())
            .insert("test".into(), state);
        map
    }

    #[test]
    fn unknown_account_returns_error() {
        let outbound = SlackOutbound::new(Arc::new(std::sync::RwLock::new(HashMap::new())));
        let err = outbound.account_snapshot("missing").err();
        assert!(err.is_some());
    }

    #[test]
    fn known_account_returns_snapshot() {
        let map = build_state_map();
        let outbound = SlackOutbound::new(map);
        let snapshot = outbound.account_snapshot("test");
        assert!(snapshot.is_ok());
    }
}
