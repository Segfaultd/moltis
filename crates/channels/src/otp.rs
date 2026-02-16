//! In-memory OTP state for self-approval of non-allowlisted DM users.
//!
//! When `dm_policy = Allowlist` and `otp_self_approval = true`, channels can
//! issue a 6-digit OTP challenge to unknown users. If they reply with the
//! correct code they are automatically added to the allowlist.

use std::{
    collections::HashMap,
    time::{Duration, Instant, SystemTime},
};

use rand::Rng;

/// How long an OTP code stays valid.
const OTP_TTL: Duration = Duration::from_secs(300);

/// Maximum wrong-code attempts before lockout.
const MAX_ATTEMPTS: u32 = 3;

/// Per-account OTP state.
pub struct OtpState {
    challenges: HashMap<String, OtpChallenge>,
    lockouts: HashMap<String, Lockout>,
    cooldown: Duration,
}

/// A pending OTP challenge for a single peer.
pub struct OtpChallenge {
    pub code: String,
    pub peer_id: String,
    pub username: Option<String>,
    pub sender_name: Option<String>,
    pub expires_at: Instant,
    pub attempts: u32,
}

/// Lockout state after too many failed attempts.
struct Lockout {
    until: Instant,
}

/// Result of initiating a challenge.
#[derive(Debug, PartialEq, Eq)]
pub enum OtpInitResult {
    /// Challenge created; contains the 6-digit code.
    Created(String),
    /// A challenge already exists for this peer.
    AlreadyPending,
    /// Peer is locked out.
    LockedOut,
}

/// Result of verifying a code.
#[derive(Debug, PartialEq, Eq)]
pub enum OtpVerifyResult {
    /// Code matched — peer should be approved.
    Approved,
    /// Wrong code; `attempts_left` remaining before lockout.
    WrongCode { attempts_left: u32 },
    /// Peer is locked out after too many failures.
    LockedOut,
    /// No pending challenge for this peer.
    NoPending,
    /// The challenge has expired.
    Expired,
}

/// Snapshot of a pending challenge for external consumers (API/UI).
#[derive(Debug, Clone, serde::Serialize)]
pub struct OtpChallengeInfo {
    pub peer_id: String,
    pub username: Option<String>,
    pub sender_name: Option<String>,
    pub code: String,
    pub expires_at: i64,
}

impl OtpState {
    #[must_use]
    pub fn new(cooldown_secs: u64) -> Self {
        Self {
            challenges: HashMap::new(),
            lockouts: HashMap::new(),
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    /// Initiate an OTP challenge for `peer_id`.
    pub fn initiate(
        &mut self,
        peer_id: &str,
        username: Option<String>,
        sender_name: Option<String>,
    ) -> OtpInitResult {
        let now = Instant::now();

        // Check lockout first.
        if let Some(lockout) = self.lockouts.get(peer_id) {
            if now < lockout.until {
                return OtpInitResult::LockedOut;
            }
            self.lockouts.remove(peer_id);
        }

        // Check for existing unexpired challenge.
        if let Some(existing) = self.challenges.get(peer_id) {
            if now < existing.expires_at {
                return OtpInitResult::AlreadyPending;
            }
            self.challenges.remove(peer_id);
        }

        let code = generate_otp_code();
        let challenge = OtpChallenge {
            code: code.clone(),
            peer_id: peer_id.to_string(),
            username,
            sender_name,
            expires_at: now + OTP_TTL,
            attempts: 0,
        };
        self.challenges.insert(peer_id.to_string(), challenge);
        OtpInitResult::Created(code)
    }

    /// Verify a code submitted by `peer_id`.
    pub fn verify(&mut self, peer_id: &str, code: &str) -> OtpVerifyResult {
        let now = Instant::now();

        if let Some(lockout) = self.lockouts.get(peer_id) {
            if now < lockout.until {
                return OtpVerifyResult::LockedOut;
            }
            self.lockouts.remove(peer_id);
        }

        let challenge = match self.challenges.get_mut(peer_id) {
            Some(c) => c,
            None => return OtpVerifyResult::NoPending,
        };

        if now >= challenge.expires_at {
            self.challenges.remove(peer_id);
            return OtpVerifyResult::Expired;
        }

        if challenge.code == code {
            self.challenges.remove(peer_id);
            return OtpVerifyResult::Approved;
        }

        challenge.attempts += 1;
        if challenge.attempts >= MAX_ATTEMPTS {
            self.challenges.remove(peer_id);
            self.lockouts.insert(
                peer_id.to_string(),
                Lockout {
                    until: now + self.cooldown,
                },
            );
            return OtpVerifyResult::LockedOut;
        }

        OtpVerifyResult::WrongCode {
            attempts_left: MAX_ATTEMPTS - challenge.attempts,
        }
    }

    #[must_use]
    pub fn has_pending(&self, peer_id: &str) -> bool {
        self.challenges
            .get(peer_id)
            .is_some_and(|c| Instant::now() < c.expires_at)
    }

    #[must_use]
    pub fn is_locked_out(&self, peer_id: &str) -> bool {
        self.lockouts
            .get(peer_id)
            .is_some_and(|l| Instant::now() < l.until)
    }

    #[must_use]
    pub fn list_pending(&self) -> Vec<OtpChallengeInfo> {
        let now_instant = Instant::now();
        let now_system = SystemTime::now();

        self.challenges
            .values()
            .filter(|c| now_instant < c.expires_at)
            .map(|c| {
                let remaining = c.expires_at.saturating_duration_since(now_instant);
                let expires_epoch = now_system
                    .checked_add(remaining)
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);

                OtpChallengeInfo {
                    peer_id: c.peer_id.clone(),
                    username: c.username.clone(),
                    sender_name: c.sender_name.clone(),
                    code: c.code.clone(),
                    expires_at: expires_epoch,
                }
            })
            .collect()
    }

    /// Remove expired challenges and elapsed lockouts.
    pub fn evict_expired(&mut self) {
        let now = Instant::now();
        self.challenges.retain(|_, c| now < c.expires_at);
        self.lockouts.retain(|_, l| now < l.until);
    }
}

fn generate_otp_code() -> String {
    let code: u32 = rand::rng().random_range(100_000..1_000_000);
    code.to_string()
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_correct_code() {
        let mut state = OtpState::new(300);
        let code = match state.initiate("user1", None, None) {
            OtpInitResult::Created(c) => c,
            _ => String::new(),
        };
        assert_eq!(state.verify("user1", &code), OtpVerifyResult::Approved);
        assert!(!state.has_pending("user1"));
    }
}
