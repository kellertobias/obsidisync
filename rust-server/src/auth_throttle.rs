//! In-memory failed-login throttle for the WebDAV endpoint.
//!
//! Device passwords carry roughly 100 bits of entropy, so guessing is hopeless in practice; the
//! throttle exists to keep an internet-facing `/dav/` from being hammered and to make a
//! misconfigured client fail loudly instead of retrying forever. Two keys are tracked per
//! attempt: the client IP (tight limit) and the username (loose limit, so a spoofed
//! `X-Forwarded-For` cannot bypass throttling entirely, while an attacker cannot easily lock the
//! real user out either).

use crate::time_format::unix_now;
use std::collections::HashMap;
use tokio::sync::Mutex;

pub const IP_FAILURE_LIMIT: u32 = 20;
pub const USER_FAILURE_LIMIT: u32 = 100;
const WINDOW_SECONDS: u64 = 15 * 60;
const LOCK_SECONDS: u64 = 15 * 60;
const MAX_TRACKED_KEYS: usize = 10_000;

#[derive(Debug, Default)]
pub struct AuthThrottle {
    entries: Mutex<HashMap<String, FailureRecord>>,
}

#[derive(Debug, Clone)]
struct FailureRecord {
    failures: u32,
    window_started_at: u64,
    locked_until: Option<u64>,
}

impl AuthThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ip_key(client_ip: &str) -> String {
        format!("ip:{client_ip}")
    }

    pub fn user_key(username: &str) -> String {
        format!("user:{}", username.trim().to_ascii_lowercase())
    }

    /// Seconds until the most restrictive key unlocks, or `None` when attempts are allowed.
    pub async fn blocked_for(&self, keys: &[String]) -> Option<u64> {
        let now = unix_now();
        let entries = self.entries.lock().await;
        keys.iter()
            .filter_map(|key| entries.get(key))
            .filter_map(|record| record.locked_until)
            .filter(|until| *until > now)
            .map(|until| until - now)
            .max()
    }

    pub async fn record_failure(&self, keys: &[String]) {
        let now = unix_now();
        let mut entries = self.entries.lock().await;
        if entries.len() >= MAX_TRACKED_KEYS {
            entries.retain(|_, record| {
                record.locked_until.is_some_and(|until| until > now)
                    || now.saturating_sub(record.window_started_at) < WINDOW_SECONDS
            });
        }
        for key in keys {
            let limit = limit_for(key);
            let record = entries.entry(key.clone()).or_insert(FailureRecord {
                failures: 0,
                window_started_at: now,
                locked_until: None,
            });
            if now.saturating_sub(record.window_started_at) >= WINDOW_SECONDS {
                record.failures = 0;
                record.window_started_at = now;
            }
            record.failures += 1;
            if record.failures >= limit {
                record.locked_until = Some(now + LOCK_SECONDS);
                record.failures = 0;
                record.window_started_at = now;
            }
        }
    }

    pub async fn record_success(&self, keys: &[String]) {
        let mut entries = self.entries.lock().await;
        for key in keys {
            entries.remove(key);
        }
    }
}

fn limit_for(key: &str) -> u32 {
    if key.starts_with("user:") {
        USER_FAILURE_LIMIT
    } else {
        IP_FAILURE_LIMIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn locks_ip_after_repeated_failures_and_clears_on_success() {
        let throttle = AuthThrottle::new();
        let keys = vec![
            AuthThrottle::ip_key("203.0.113.5"),
            AuthThrottle::user_key("Alice"),
        ];
        for _ in 0..IP_FAILURE_LIMIT - 1 {
            throttle.record_failure(&keys).await;
        }
        assert_eq!(throttle.blocked_for(&keys).await, None);
        throttle.record_failure(&keys).await;
        let remaining = throttle.blocked_for(&keys).await.unwrap();
        assert!(remaining > 0 && remaining <= LOCK_SECONDS);

        let other_ip = vec![
            AuthThrottle::ip_key("203.0.113.6"),
            AuthThrottle::user_key("alice"),
        ];
        assert_eq!(throttle.blocked_for(&other_ip).await, None);

        throttle.record_success(&keys).await;
        assert_eq!(throttle.blocked_for(&keys).await, None);
    }
}
