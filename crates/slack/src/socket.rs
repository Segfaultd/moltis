use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::{
    handlers::{SlackInboundMessage, handle_inbound_message},
    state::AccountStateMap,
};

#[derive(Debug, serde::Deserialize)]
struct ConnectionsOpenResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct SocketEnvelope {
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Option<EnvelopePayload>,
}

#[derive(Debug, serde::Deserialize)]
struct EnvelopePayload {
    #[serde(default)]
    event: Option<SlackEvent>,
}

#[derive(Debug, serde::Deserialize)]
struct SlackEvent {
    #[serde(default, rename = "type")]
    event_type: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    channel_type: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

pub async fn start_socket_mode(
    account_id: String,
    accounts: AccountStateMap,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    info!(account_id, "starting slack socket mode loop");

    loop {
        if cancel.is_cancelled() {
            info!(account_id, "slack socket mode stopped");
            break;
        }

        let (app_token, bot_user_id) = {
            let guard = accounts.read().unwrap_or_else(|e| e.into_inner());
            let state = guard
                .get(&account_id)
                .ok_or_else(|| anyhow!("account state missing: {account_id}"))?;
            (
                state.config.app_token.expose_secret().clone(),
                state.bot_user_id.clone(),
            )
        };

        match open_socket_url(&app_token).await {
            Ok(url) => match run_socket_session(
                &account_id,
                &accounts,
                &cancel,
                &url,
                bot_user_id.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        break;
                    }
                    warn!(account_id, "slack socket session ended, reconnecting");
                },
                Err(e) => {
                    warn!(account_id, error = %e, "slack socket session failed, reconnecting");
                },
            },
            Err(e) => {
                warn!(account_id, error = %e, "failed to open slack socket URL");
            },
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    Ok(())
}

async fn open_socket_url(app_token: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://slack.com/api/apps.connections.open")
        .bearer_auth(app_token)
        .send()
        .await?;

    let body: ConnectionsOpenResponse = resp.json().await?;
    if !body.ok {
        return Err(anyhow!(
            "apps.connections.open failed: {}",
            body.error.unwrap_or_else(|| "unknown error".to_string())
        ));
    }
    body.url
        .ok_or_else(|| anyhow!("apps.connections.open response missing url"))
}

async fn run_socket_session(
    account_id: &str,
    accounts: &AccountStateMap,
    cancel: &tokio_util::sync::CancellationToken,
    ws_url: &str,
    bot_user_id: Option<&str>,
) -> Result<()> {
    let (ws, _) = connect_async(ws_url).await?;
    let (mut sink, mut stream) = ws.split();

    while let Some(frame) = stream.next().await {
        if cancel.is_cancelled() {
            break;
        }

        let msg = match frame {
            Ok(m) => m,
            Err(e) => return Err(anyhow!("websocket read error: {e}")),
        };

        if let Message::Text(text) = msg {
            let parsed: SocketEnvelope = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    debug!(account_id, error = %e, "ignoring unparseable slack socket frame");
                    continue;
                },
            };

            if let Some(envelope_id) = parsed.envelope_id {
                let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
                sink.send(Message::Text(ack.into())).await?;
            }

            let Some(event) = parsed.payload.and_then(|p| p.event) else {
                continue;
            };

            if event.event_type != "message" && event.event_type != "app_mention" {
                continue;
            }

            // Ignore bot/system generated messages.
            if event.subtype.is_some() {
                continue;
            }

            let user_id = event.user.unwrap_or_default();
            let channel_id = event.channel.unwrap_or_default();
            let text = event.text.unwrap_or_default();
            let ts = event.ts.unwrap_or_default();
            if user_id.is_empty() || channel_id.is_empty() || ts.is_empty() {
                continue;
            }

            let bot_mentioned = bot_user_id
                .map(|id| text.contains(&format!("<@{id}>")))
                .unwrap_or(false)
                || event.event_type == "app_mention";

            let inbound = SlackInboundMessage {
                user_id,
                username: None,
                sender_name: None,
                channel_id,
                channel_type: event.channel_type.unwrap_or_else(|| "channel".to_string()),
                ts,
                thread_ts: event.thread_ts,
                text,
                bot_mentioned,
            };

            if let Err(e) = handle_inbound_message(accounts, account_id, inbound).await {
                warn!(account_id, error = %e, "failed to handle slack inbound message");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_socket_event_payload() {
        let raw = r#"{"envelope_id":"1","payload":{"event":{"type":"message","user":"U1","channel":"C1","text":"hello","ts":"123.45","channel_type":"im"}}}"#;
        let parsed: super::SocketEnvelope =
            serde_json::from_str(raw)
                .ok()
                .unwrap_or(super::SocketEnvelope {
                    envelope_id: None,
                    payload: None,
                });
        assert!(parsed.envelope_id.is_some());
    }
}
