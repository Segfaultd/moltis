use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use tokio_util::sync::CancellationToken;

use moltis_channels::{ChannelEventSink, message_log::MessageLog, otp::OtpState};

use crate::{config::SlackAccountConfig, outbound::SlackOutbound};

pub type AccountStateMap = Arc<RwLock<HashMap<String, AccountState>>>;

pub struct AccountState {
    pub account_id: String,
    pub config: SlackAccountConfig,
    pub bot_user_id: Option<String>,
    pub outbound: Arc<SlackOutbound>,
    pub cancel: CancellationToken,
    pub message_log: Option<Arc<dyn MessageLog>>,
    pub event_sink: Option<Arc<dyn ChannelEventSink>>,
    pub otp: Mutex<OtpState>,
}
