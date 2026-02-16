use moltis_channels::gating::{self, DmPolicy, GroupPolicy, MentionMode};

use crate::config::SlackAccountConfig;

#[derive(Debug, Clone, Copy)]
pub enum ChatType {
    Dm,
    Channel,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AccessDenied {
    DmDisabled,
    DmNotAllowed,
    ChannelDisabled,
    ChannelNotAllowed,
    MentionRequired,
    MentionModeNone,
}

pub fn check_access(
    config: &SlackAccountConfig,
    chat_type: ChatType,
    peer_or_channel_id: &str,
    bot_mentioned: bool,
) -> Result<(), AccessDenied> {
    match chat_type {
        ChatType::Dm => match config.dm_policy {
            DmPolicy::Disabled => Err(AccessDenied::DmDisabled),
            DmPolicy::Open => Ok(()),
            DmPolicy::Allowlist => {
                if config.allowlist.is_empty() {
                    return Err(AccessDenied::DmNotAllowed);
                }
                if gating::is_allowed(peer_or_channel_id, &config.allowlist) {
                    Ok(())
                } else {
                    Err(AccessDenied::DmNotAllowed)
                }
            },
        },
        ChatType::Channel => {
            match config.channel_policy {
                GroupPolicy::Disabled => return Err(AccessDenied::ChannelDisabled),
                GroupPolicy::Open => {},
                GroupPolicy::Allowlist => {
                    if config.channel_allowlist.is_empty()
                        || !gating::is_allowed(peer_or_channel_id, &config.channel_allowlist)
                    {
                        return Err(AccessDenied::ChannelNotAllowed);
                    }
                },
            }
            match config.mention_mode {
                MentionMode::Always => Ok(()),
                MentionMode::None => Err(AccessDenied::MentionModeNone),
                MentionMode::Mention => {
                    if bot_mentioned {
                        Ok(())
                    } else {
                        Err(AccessDenied::MentionRequired)
                    }
                },
            }
        },
    }
}

impl std::fmt::Display for AccessDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DmDisabled => f.write_str("DMs are disabled"),
            Self::DmNotAllowed => f.write_str("user not on allowlist"),
            Self::ChannelDisabled => f.write_str("channels are disabled"),
            Self::ChannelNotAllowed => f.write_str("channel not on allowlist"),
            Self::MentionRequired => f.write_str("bot mention is required"),
            Self::MentionModeNone => f.write_str("bot does not respond in channels"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_open_allows_all() {
        let cfg = SlackAccountConfig {
            dm_policy: DmPolicy::Open,
            ..Default::default()
        };
        assert!(check_access(&cfg, ChatType::Dm, "U123", false).is_ok());
    }

    #[test]
    fn dm_disabled_denies() {
        let cfg = SlackAccountConfig {
            dm_policy: DmPolicy::Disabled,
            ..Default::default()
        };
        assert!(check_access(&cfg, ChatType::Dm, "U123", false).is_err());
    }

    #[test]
    fn dm_allowlist_allows_listed() {
        let cfg = SlackAccountConfig {
            dm_policy: DmPolicy::Allowlist,
            allowlist: vec!["U123".into()],
            ..Default::default()
        };
        assert!(check_access(&cfg, ChatType::Dm, "U123", false).is_ok());
        assert!(check_access(&cfg, ChatType::Dm, "U999", false).is_err());
    }

    #[test]
    fn dm_empty_allowlist_denies_all() {
        let cfg = SlackAccountConfig {
            dm_policy: DmPolicy::Allowlist,
            allowlist: vec![],
            ..Default::default()
        };
        assert!(check_access(&cfg, ChatType::Dm, "U123", false).is_err());
    }

    #[test]
    fn channel_mention_mode() {
        let cfg = SlackAccountConfig {
            channel_policy: GroupPolicy::Open,
            mention_mode: MentionMode::Mention,
            ..Default::default()
        };
        assert!(check_access(&cfg, ChatType::Channel, "C123", true).is_ok());
        assert!(check_access(&cfg, ChatType::Channel, "C123", false).is_err());
    }
}
