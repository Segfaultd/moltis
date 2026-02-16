use {
    moltis_channels::gating::{DmPolicy, GroupPolicy, MentionMode},
    secrecy::{ExposeSecret, Secret},
    serde::{Deserialize, Serialize},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    #[default]
    EditInPlace,
    Off,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackAccountConfig {
    #[serde(serialize_with = "serialize_secret")]
    pub app_token: Secret<String>,
    #[serde(serialize_with = "serialize_secret")]
    pub bot_token: Secret<String>,
    pub dm_policy: DmPolicy,
    pub channel_policy: GroupPolicy,
    pub mention_mode: MentionMode,
    pub allowlist: Vec<String>,
    pub channel_allowlist: Vec<String>,
    pub stream_mode: StreamMode,
    pub edit_throttle_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    pub otp_self_approval: bool,
    pub otp_cooldown_secs: u64,
    pub chunk_size: usize,
}

impl std::fmt::Debug for SlackAccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackAccountConfig")
            .field("app_token", &"[REDACTED]")
            .field("bot_token", &"[REDACTED]")
            .field("dm_policy", &self.dm_policy)
            .field("channel_policy", &self.channel_policy)
            .field("mention_mode", &self.mention_mode)
            .finish_non_exhaustive()
    }
}

fn serialize_secret<S: serde::Serializer>(
    secret: &Secret<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(secret.expose_secret())
}

impl Default for SlackAccountConfig {
    fn default() -> Self {
        Self {
            app_token: Secret::new(String::new()),
            bot_token: Secret::new(String::new()),
            dm_policy: DmPolicy::default(),
            channel_policy: GroupPolicy::default(),
            mention_mode: MentionMode::default(),
            allowlist: Vec::new(),
            channel_allowlist: Vec::new(),
            stream_mode: StreamMode::default(),
            edit_throttle_ms: 1000,
            model: None,
            model_provider: None,
            otp_self_approval: true,
            otp_cooldown_secs: 300,
            chunk_size: 4000,
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let cfg = SlackAccountConfig::default();
        assert_eq!(cfg.dm_policy, DmPolicy::Open);
        assert_eq!(cfg.channel_policy, GroupPolicy::Open);
        assert_eq!(cfg.mention_mode, MentionMode::Mention);
        assert_eq!(cfg.edit_throttle_ms, 1000);
        assert_eq!(cfg.chunk_size, 4000);
        assert!(cfg.otp_self_approval);
    }

    #[test]
    fn config_roundtrip_json() {
        let cfg = SlackAccountConfig {
            app_token: Secret::new("xapp-test".into()),
            bot_token: Secret::new("xoxb-test".into()),
            dm_policy: DmPolicy::Allowlist,
            allowlist: vec!["U12345".into()],
            ..Default::default()
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["dm_policy"], "allowlist");
        assert_eq!(json["allowlist"][0], "U12345");
        assert_eq!(json["app_token"], "xapp-test");
    }

    #[test]
    fn debug_redacts_tokens() {
        let cfg = SlackAccountConfig::default();
        let debug = format!("{cfg:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("xapp-"));
        assert!(!debug.contains("xoxb-"));
    }
}
