use std::sync::Arc;

use tracing::{info, warn};

use moltis_channels::{
    ChannelEvent, ChannelMessageMeta, ChannelOutbound, ChannelReplyTarget, ChannelType,
    message_log::MessageLogEntry,
    otp::{OtpInitResult, OtpVerifyResult},
};

use crate::{
    access::{self, AccessDenied, ChatType},
    state::AccountStateMap,
};

pub const OTP_CHALLENGE_MSG: &str = "To use this bot, enter the verification code. Ask the bot owner for the code from Channels -> Senders. The code expires in 5 minutes.";

#[derive(Debug, Clone)]
pub struct SlackInboundMessage {
    pub user_id: String,
    pub username: Option<String>,
    pub sender_name: Option<String>,
    pub channel_id: String,
    pub channel_type: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub text: String,
    pub bot_mentioned: bool,
}

pub async fn handle_inbound_message(
    accounts: &AccountStateMap,
    account_id: &str,
    msg: SlackInboundMessage,
) -> anyhow::Result<()> {
    let (config, message_log, event_sink) = {
        let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
        let state = accts
            .get(account_id)
            .ok_or_else(|| anyhow::anyhow!("account not found: {account_id}"))?;
        (
            state.config.clone(),
            state.message_log.clone(),
            state.event_sink.clone(),
        )
    };

    let chat_type = if msg.channel_type == "im" {
        ChatType::Dm
    } else {
        ChatType::Channel
    };

    let access_result = access::check_access(
        &config,
        chat_type,
        if matches!(chat_type, ChatType::Dm) {
            &msg.user_id
        } else {
            &msg.channel_id
        },
        msg.bot_mentioned,
    );
    let access_granted = access_result.is_ok();

    if let Some(log) = &message_log {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let entry = MessageLogEntry {
            id: 0,
            account_id: account_id.to_string(),
            channel_type: ChannelType::Slack.to_string(),
            peer_id: msg.user_id.clone(),
            username: msg.username.clone(),
            sender_name: msg.sender_name.clone(),
            chat_id: msg.channel_id.clone(),
            chat_type: if matches!(chat_type, ChatType::Dm) {
                "dm".into()
            } else {
                "channel".into()
            },
            body: msg.text.clone(),
            access_granted,
            created_at: now,
        };
        if let Err(e) = log.log(entry).await {
            warn!(account_id, "failed to log slack message: {e}");
        }
    }

    if let Some(sink) = &event_sink {
        sink.emit(ChannelEvent::InboundMessage {
            channel_type: ChannelType::Slack,
            account_id: account_id.to_string(),
            peer_id: msg.user_id.clone(),
            username: msg.username.clone(),
            sender_name: msg.sender_name.clone(),
            message_count: None,
            access_granted,
        })
        .await;
    }

    if let Err(reason) = access_result {
        if reason == AccessDenied::DmNotAllowed
            && matches!(chat_type, ChatType::Dm)
            && config.otp_self_approval
        {
            handle_otp_flow(accounts, account_id, &msg, event_sink.as_deref()).await;
        }
        return Ok(());
    }

    let reply_target = ChannelReplyTarget {
        channel_type: ChannelType::Slack,
        account_id: account_id.to_string(),
        chat_id: msg.channel_id.clone(),
        message_id: Some(msg.thread_ts.clone().unwrap_or_else(|| msg.ts.clone())),
    };

    let text = msg.text.trim();
    if text.starts_with('/') {
        let cmd_text = text.trim_start_matches('/');
        let cmd = cmd_text.split_whitespace().next().unwrap_or("");
        if matches!(
            cmd,
            "new" | "clear" | "compact" | "context" | "model" | "sandbox" | "sessions" | "help"
        ) {
            if let Some(sink) = &event_sink {
                let response = if cmd == "help" {
                    "Available commands:\n/new\n/sessions\n/model\n/sandbox\n/clear\n/compact\n/context\n/help".to_string()
                } else {
                    match sink.dispatch_command(cmd_text, reply_target.clone()).await {
                        Ok(v) => v,
                        Err(e) => format!("Error: {e}"),
                    }
                };
                let outbound = {
                    let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
                    accts.get(account_id).map(|s| Arc::clone(&s.outbound))
                };
                if let Some(outbound) = outbound {
                    let _ = outbound
                        .send_text(
                            account_id,
                            &reply_target.chat_id,
                            &response,
                            reply_target.message_id.as_deref(),
                        )
                        .await;
                }
            }
            return Ok(());
        }
    }

    if let Some(sink) = &event_sink {
        let meta = ChannelMessageMeta {
            channel_type: ChannelType::Slack,
            sender_name: msg.sender_name,
            username: msg.username,
            message_kind: Some(moltis_channels::ChannelMessageKind::Text),
            model: config.model,
        };
        info!(
            account_id,
            channel_id = reply_target.chat_id,
            "dispatching slack message to chat"
        );
        sink.dispatch_to_chat(text, reply_target, meta).await;
    }

    Ok(())
}

async fn handle_otp_flow(
    accounts: &AccountStateMap,
    account_id: &str,
    msg: &SlackInboundMessage,
    event_sink: Option<&dyn moltis_channels::ChannelEventSink>,
) {
    let outbound = {
        let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
        accts.get(account_id).map(|s| Arc::clone(&s.outbound))
    };
    let Some(outbound) = outbound else {
        return;
    };

    let has_pending = {
        let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
        accts
            .get(account_id)
            .map(|s| {
                let otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                otp.has_pending(&msg.user_id)
            })
            .unwrap_or(false)
    };

    if has_pending {
        let body = msg.text.trim();
        let is_code = body.len() == 6 && body.chars().all(|c| c.is_ascii_digit());
        if !is_code {
            return;
        }

        let result = {
            let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
            match accts.get(account_id) {
                Some(s) => {
                    let mut otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                    otp.verify(&msg.user_id, body)
                },
                None => return,
            }
        };

        match result {
            OtpVerifyResult::Approved => {
                if let Some(sink) = event_sink {
                    sink.request_sender_approval("slack", account_id, &msg.user_id)
                        .await;
                    sink.emit(ChannelEvent::OtpResolved {
                        channel_type: ChannelType::Slack,
                        account_id: account_id.to_string(),
                        peer_id: msg.user_id.clone(),
                        username: msg.username.clone(),
                        resolution: "approved".into(),
                    })
                    .await;
                }
                let _ = outbound
                    .send_text(
                        account_id,
                        &msg.channel_id,
                        "Verified! You now have access.",
                        msg.thread_ts.as_deref(),
                    )
                    .await;
            },
            OtpVerifyResult::WrongCode { attempts_left } => {
                let _ = outbound
                    .send_text(
                        account_id,
                        &msg.channel_id,
                        &format!("Incorrect code. {attempts_left} attempts remaining."),
                        msg.thread_ts.as_deref(),
                    )
                    .await;
            },
            OtpVerifyResult::LockedOut => {
                let _ = outbound
                    .send_text(
                        account_id,
                        &msg.channel_id,
                        "Too many failed attempts. Try again later.",
                        msg.thread_ts.as_deref(),
                    )
                    .await;
            },
            OtpVerifyResult::Expired => {
                let _ = outbound
                    .send_text(
                        account_id,
                        &msg.channel_id,
                        "Code expired. Send a message to receive a new one.",
                        msg.thread_ts.as_deref(),
                    )
                    .await;
            },
            OtpVerifyResult::NoPending => {},
        }

        return;
    }

    let challenge_result = {
        let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
        match accts.get(account_id) {
            Some(s) => {
                let mut otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                otp.initiate(&msg.user_id, msg.username.clone(), msg.sender_name.clone())
            },
            None => return,
        }
    };

    match challenge_result {
        OtpInitResult::Created(_) | OtpInitResult::AlreadyPending => {
            let _ = outbound
                .send_text(
                    account_id,
                    &msg.channel_id,
                    OTP_CHALLENGE_MSG,
                    msg.thread_ts.as_deref(),
                )
                .await;

            if let Some(sink) = event_sink {
                let otp_info = {
                    let accts = accounts.read().unwrap_or_else(|e| e.into_inner());
                    accts
                        .get(account_id)
                        .map(|s| {
                            let otp = s.otp.lock().unwrap_or_else(|e| e.into_inner());
                            otp.list_pending()
                                .into_iter()
                                .find(|c| c.peer_id == msg.user_id)
                        })
                        .flatten()
                };
                if let Some(c) = otp_info {
                    sink.emit(ChannelEvent::OtpChallenge {
                        channel_type: ChannelType::Slack,
                        account_id: account_id.to_string(),
                        peer_id: c.peer_id,
                        username: c.username,
                        sender_name: c.sender_name,
                        code: c.code,
                        expires_at: c.expires_at,
                    })
                    .await;
                }
            }
        },
        OtpInitResult::LockedOut => {
            let _ = outbound
                .send_text(
                    account_id,
                    &msg.channel_id,
                    "Too many failed attempts. Try again later.",
                    msg.thread_ts.as_deref(),
                )
                .await;
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use secrecy::Secret;

    use super::*;
    use crate::{config::SlackAccountConfig, outbound::SlackOutbound, state::AccountState};
    use moltis_channels::{ChannelEventSink, message_log::MessageLog, otp::OtpState};

    struct TestSink {
        dispatched: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ChannelEventSink for TestSink {
        async fn emit(&self, _event: ChannelEvent) {}

        async fn dispatch_to_chat(
            &self,
            text: &str,
            _reply_to: ChannelReplyTarget,
            _meta: ChannelMessageMeta,
        ) {
            let mut guard = self.dispatched.lock().unwrap_or_else(|e| e.into_inner());
            guard.push(text.to_string());
        }

        async fn dispatch_command(
            &self,
            command: &str,
            _reply_to: ChannelReplyTarget,
        ) -> anyhow::Result<String> {
            Ok(format!("cmd:{command}"))
        }

        async fn request_disable_account(
            &self,
            _channel_type: &str,
            _account_id: &str,
            _reason: &str,
        ) {
        }
    }

    fn build_state(
        dm_policy: moltis_channels::gating::DmPolicy,
        otp_self_approval: bool,
    ) -> (AccountStateMap, Arc<TestSink>) {
        let sink = Arc::new(TestSink {
            dispatched: Mutex::new(Vec::new()),
        });
        let map = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let outbound = Arc::new(SlackOutbound::new(Arc::clone(&map)));
        let state = AccountState {
            account_id: "a1".into(),
            config: SlackAccountConfig {
                app_token: Secret::new("xapp".into()),
                bot_token: Secret::new("xoxb".into()),
                dm_policy,
                otp_self_approval,
                ..Default::default()
            },
            bot_user_id: Some("U_BOT".into()),
            outbound,
            cancel: tokio_util::sync::CancellationToken::new(),
            message_log: None::<Arc<dyn MessageLog>>,
            event_sink: Some(sink.clone()),
            otp: Mutex::new(OtpState::new(300)),
        };
        map.write()
            .unwrap_or_else(|e| e.into_inner())
            .insert("a1".into(), state);
        (map, sink)
    }

    #[tokio::test]
    async fn dm_open_dispatches_message() {
        let (accounts, sink) = build_state(moltis_channels::gating::DmPolicy::Open, false);
        let msg = SlackInboundMessage {
            user_id: "U1".into(),
            username: Some("alice".into()),
            sender_name: Some("Alice".into()),
            channel_id: "D1".into(),
            channel_type: "im".into(),
            ts: "123.1".into(),
            thread_ts: None,
            text: "hello".into(),
            bot_mentioned: false,
        };
        let res = handle_inbound_message(&accounts, "a1", msg).await;
        assert!(res.is_ok());
        let dispatched = sink
            .dispatched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0], "hello");
    }

    #[tokio::test]
    async fn dm_allowlist_denied_without_otp_does_not_dispatch() {
        let (accounts, sink) = build_state(moltis_channels::gating::DmPolicy::Allowlist, false);
        let msg = SlackInboundMessage {
            user_id: "U1".into(),
            username: None,
            sender_name: None,
            channel_id: "D1".into(),
            channel_type: "im".into(),
            ts: "123.1".into(),
            thread_ts: None,
            text: "hello".into(),
            bot_mentioned: false,
        };
        let res = handle_inbound_message(&accounts, "a1", msg).await;
        assert!(res.is_ok());
        let dispatched = sink
            .dispatched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert!(dispatched.is_empty());
    }
}
