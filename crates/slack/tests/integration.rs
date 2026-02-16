use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use secrecy::Secret;

use moltis_channels::{
    ChannelEvent, ChannelEventSink, ChannelMessageMeta, ChannelReplyTarget, message_log::MessageLog,
};
use moltis_slack::{
    config::SlackAccountConfig,
    handlers::{SlackInboundMessage, handle_inbound_message},
    outbound::SlackOutbound,
    state::{AccountState, AccountStateMap},
};

struct Sink {
    msgs: Mutex<Vec<String>>,
}

#[async_trait]
impl ChannelEventSink for Sink {
    async fn emit(&self, _event: ChannelEvent) {}

    async fn dispatch_to_chat(
        &self,
        text: &str,
        _reply_to: ChannelReplyTarget,
        _meta: ChannelMessageMeta,
    ) {
        self.msgs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text.to_string());
    }

    async fn dispatch_command(
        &self,
        _command: &str,
        _reply_to: ChannelReplyTarget,
    ) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }

    async fn request_disable_account(&self, _channel_type: &str, _account_id: &str, _reason: &str) {
    }
}

fn build_state() -> (AccountStateMap, Arc<Sink>) {
    let sink = Arc::new(Sink {
        msgs: Mutex::new(Vec::new()),
    });

    let accounts: AccountStateMap = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let outbound = Arc::new(SlackOutbound::new(Arc::clone(&accounts)));
    let state = AccountState {
        account_id: "acct".to_string(),
        config: SlackAccountConfig {
            app_token: Secret::new("xapp-test".to_string()),
            bot_token: Secret::new("xoxb-test".to_string()),
            ..Default::default()
        },
        bot_user_id: Some("U_BOT".to_string()),
        outbound,
        cancel: tokio_util::sync::CancellationToken::new(),
        message_log: None::<Arc<dyn MessageLog>>,
        event_sink: Some(sink.clone()),
        otp: std::sync::Mutex::new(moltis_channels::otp::OtpState::new(300)),
    };
    accounts
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert("acct".to_string(), state);
    (accounts, sink)
}

#[tokio::test]
async fn inbound_message_dispatches_to_chat() {
    let (accounts, sink) = build_state();
    let msg = SlackInboundMessage {
        user_id: "U1".to_string(),
        username: Some("alice".to_string()),
        sender_name: Some("Alice".to_string()),
        channel_id: "D1".to_string(),
        channel_type: "im".to_string(),
        ts: "123.1".to_string(),
        thread_ts: None,
        text: "hello from slack".to_string(),
        bot_mentioned: false,
    };

    let res = handle_inbound_message(&accounts, "acct", msg).await;
    assert!(res.is_ok());

    let msgs = sink.msgs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(msgs, vec!["hello from slack".to_string()]);
}
