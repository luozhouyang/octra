//! Virtual key hashing and authentication.

use chrono::{DateTime, Utc};
use octra_storage::LlmVirtualKeyRecord;
use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "octra-";

/// SHA-256 of the plaintext key, lowercase hex.
pub fn hash_key(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    hex::encode(digest)
}

/// Generates a new key. The plaintext is returned once; only the prefix and
/// hash are persisted.
pub fn generate_key() -> String {
    format!(
        "{}{}{}",
        KEY_PREFIX,
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub fn display_prefix(plaintext: &str) -> String {
    plaintext.chars().take(12).collect()
}

/// Extracts the presented key from `Authorization: Bearer` or `x-api-key`.
pub fn presented_key(authorization: Option<&str>, api_key: Option<&str>) -> Option<String> {
    if let Some(value) = authorization {
        let value = value.trim();
        if let Some(token) = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
        {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// No keys are configured: anonymous local access.
    Anonymous,
    /// A valid, unexpired key matched.
    Authorized(LlmVirtualKeyRecord),
    /// A key was presented but is unknown, disabled or expired.
    Rejected(&'static str),
}

/// Whether any key could ever be valid right now. Keys that are disabled or
/// expired do not lock the gateway: with no live key, local access stays open.
pub fn has_live_keys(keys: &[LlmVirtualKeyRecord], now: DateTime<Utc>) -> bool {
    keys.iter().any(|key| key.enabled && !is_expired(key, now))
}

pub fn is_expired(key: &LlmVirtualKeyRecord, now: DateTime<Utc>) -> bool {
    key.expires_at.is_some_and(|expires| expires <= now)
}

/// Looks up the presented key by hash completeness.
pub fn authenticate(
    keys: &[LlmVirtualKeyRecord],
    presented: Option<String>,
    now: DateTime<Utc>,
) -> KeyOutcome {
    if !has_live_keys(keys, now) {
        return KeyOutcome::Anonymous;
    }
    let Some(presented) = presented else {
        return KeyOutcome::Rejected("missing virtual key");
    };
    let hash = hash_key(&presented);
    let Some(key) = keys.iter().find(|key| key.key_hash == hash) else {
        return KeyOutcome::Rejected("invalid virtual key");
    };
    if !key.enabled {
        return KeyOutcome::Rejected("virtual key is disabled");
    }
    if is_expired(key, now) {
        return KeyOutcome::Rejected("virtual key has expired");
    }
    KeyOutcome::Authorized(key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn key(hash: &str, enabled: bool, expires: Option<DateTime<Utc>>) -> LlmVirtualKeyRecord {
        LlmVirtualKeyRecord {
            id: "k1".into(),
            name: "k".into(),
            key_prefix: "octra-abc".into(),
            key_hash: hash.into(),
            consumer_id: None,
            enabled,
            expires_at: expires,
            last_used_at: None,
            permissions_json: "{}".into(),
            created_at: String::new(),
        }
    }

    #[test]
    fn hash_is_stable_hex() {
        assert_eq!(
            hash_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn generated_keys_are_unique_and_prefixed() {
        let first = generate_key();
        let second = generate_key();
        assert!(first.starts_with(KEY_PREFIX));
        assert_ne!(first, second);
        assert_eq!(display_prefix(&first).len(), 12);
    }

    #[test]
    fn extracts_presented_keys() {
        assert_eq!(
            presented_key(Some("Bearer abc"), None),
            Some("abc".to_string())
        );
        assert_eq!(presented_key(None, Some("xyz")), Some("xyz".to_string()));
        assert_eq!(presented_key(Some("Basic abc"), None), None);
        assert_eq!(presented_key(None, Some("  ")), None);
    }

    #[test]
    fn anonymous_when_no_live_keys() {
        let now = Utc::now();
        assert_eq!(authenticate(&[], None, now), KeyOutcome::Anonymous);
        let expired = key("h", true, Some(now - Duration::seconds(1)));
        assert_eq!(
            authenticate(&[expired], Some("x".into()), now),
            KeyOutcome::Anonymous
        );
        let disabled = key("h", false, None);
        assert_eq!(
            authenticate(&[disabled], Some("x".into()), now),
            KeyOutcome::Anonymous
        );
    }

    #[test]
    fn live_key_requires_the_right_secret() {
        let now = Utc::now();
        let live = key(&hash_key("secret"), true, Some(now + Duration::hours(1)));
        let keys = std::slice::from_ref(&live);
        assert_eq!(
            authenticate(keys, None, now),
            KeyOutcome::Rejected("missing virtual key")
        );
        assert_eq!(
            authenticate(keys, Some("wrong".into()), now),
            KeyOutcome::Rejected("invalid virtual key")
        );
        assert_eq!(
            authenticate(keys, Some("secret".into()), now),
            KeyOutcome::Authorized(live)
        );
    }
}
