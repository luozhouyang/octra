//! Data plane authentication: MCP API keys resolved to consumers.
//!
//! When at least one enabled key exists, every `/mcp` request must carry a
//! valid key in `Authorization: Bearer <key>` or `X-API-Key: <key>`. With no
//! keys the gateway is open to local clients, which is the default for a
//! loopback-only installation. Keys are never stored: the database keeps a
//! display prefix and the SHA-256 hex digest.

use axum::http::HeaderMap;
use octra_storage::Storage;
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "octra_";
const KEY_SECRET_BYTES: usize = 32;
const DISPLAY_PREFIX_CHARS: usize = 12;
/// SHA-256 is unsalted by design (keys are high-entropy); this is only the
/// per-row hash compared with the presented key.
pub const API_KEY_HEADER: &str = "x-api-key";

/// Who a request acts as on the data plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub consumer_id: Option<String>,
    pub key_id: Option<String>,
}

impl Caller {
    pub fn anonymous() -> Self {
        Self {
            consumer_id: None,
            key_id: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing MCP API key")]
    Missing,
    #[error("invalid MCP API key")]
    Invalid,
    #[error("MCP API key is disabled or expired")]
    Inactive,
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<octra_core::Error> for AuthError {
    fn from(err: octra_core::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

/// Extracts the presented key from the headers.
pub fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }

    headers
        .get(API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Authenticates a data plane request. Returns the anonymous caller when the
/// installation has no enabled keys.
pub async fn authenticate(storage: &Storage, headers: &HeaderMap) -> Result<Caller, AuthError> {
    // Resolution does not depend on the key-gating flag for correctness: an
    // unknown or inactive key is rejected whenever one is presented, and an
    // absent key is only tolerated when no enabled keys exist.
    let presented = presented_key(headers);
    if !storage.mcp.has_enabled_api_keys().await? {
        return match presented {
            Some(key)
                if storage
                    .mcp
                    .find_active_api_key(&hash_key(&key))
                    .await?
                    .is_some() =>
            {
                let record = storage
                    .mcp
                    .find_active_api_key(&hash_key(&key))
                    .await?
                    .expect("key resolved above");
                Ok(Caller {
                    consumer_id: record.consumer_id,
                    key_id: Some(record.id),
                })
            }
            // An invalid key is rejected even in open mode: silently ignoring
            // it would hide a misconfiguration.
            Some(_) => Err(AuthError::Invalid),
            None => Ok(Caller::anonymous()),
        };
    }

    let presented = presented.ok_or(AuthError::Missing)?;
    let hash = hash_key(&presented);
    let record = storage
        .mcp
        .find_active_api_key(&hash)
        .await?
        .ok_or(AuthError::Invalid)?;

    Ok(Caller {
        consumer_id: record.consumer_id,
        key_id: Some(record.id),
    })
}

/// SHA-256 of the key, lowercase hex.
pub fn hash_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    hex::encode(digest)
}

/// Generates a new key: `octra_` plus 64 hex characters from the OS RNG.
pub fn generate_key() -> String {
    let mut bytes = [0u8; KEY_SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);
    format!("{KEY_PREFIX}{}", hex::encode(bytes))
}

/// Prefix stored for display, enough to identify a key but not reproduce it.
pub fn display_prefix(key: &str) -> String {
    key.chars().take(DISPLAY_PREFIX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn hashes_are_stable_hex() {
        let hash = hash_key("octra_example");
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(hash, hash_key("octra_example"));
        assert_ne!(hash, hash_key("octra_examplf"));
    }

    #[test]
    fn generated_keys_are_unique_and_prefixed() {
        let first = generate_key();
        let second = generate_key();
        assert!(first.starts_with(KEY_PREFIX));
        assert_eq!(first.len(), KEY_PREFIX.len() + KEY_SECRET_BYTES * 2);
        assert_ne!(first, second);
        assert_eq!(display_prefix(&first).len(), DISPLAY_PREFIX_CHARS);
        assert!(!second.starts_with(&display_prefix(&first)));
    }

    #[test]
    fn extracts_bearer_then_api_key_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(presented_key(&headers), None);

        headers.insert(API_KEY_HEADER, HeaderValue::from_static("from-header"));
        assert_eq!(presented_key(&headers).as_deref(), Some("from-header"));

        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer from-bearer"),
        );
        assert_eq!(presented_key(&headers).as_deref(), Some("from-bearer"));
    }

    #[tokio::test]
    async fn open_mode_accepts_anonymous_and_explicit_keys() {
        let storage = Storage::connect_in_memory().await.unwrap();
        let caller = authenticate(&storage, &HeaderMap::new()).await.unwrap();
        assert_eq!(caller, Caller::anonymous());

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer octra_unknown"),
        );
        assert!(matches!(
            authenticate(&storage, &headers).await,
            Err(AuthError::Invalid)
        ));
    }

    #[tokio::test]
    async fn key_mode_requires_a_valid_key() {
        let storage = Storage::connect_in_memory().await.unwrap();
        storage
            .mcp
            .ensure_consumer("c1", "consumer-one")
            .await
            .unwrap();
        let key = generate_key();
        storage
            .mcp
            .insert_api_key(
                "agent",
                &display_prefix(&key),
                &hash_key(&key),
                Some("c1"),
                None,
            )
            .await
            .unwrap();

        assert!(matches!(
            authenticate(&storage, &HeaderMap::new()).await,
            Err(AuthError::Missing)
        ));

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer octra_wrong"),
        );
        assert!(matches!(
            authenticate(&storage, &headers).await,
            Err(AuthError::Invalid)
        ));

        let mut headers = HeaderMap::new();
        headers.insert(
            API_KEY_HEADER,
            HeaderValue::from_str(&key).expect("ascii key"),
        );
        let caller = authenticate(&storage, &headers).await.unwrap();
        assert_eq!(caller.consumer_id.as_deref(), Some("c1"));
        assert!(caller.key_id.is_some());
    }
}
