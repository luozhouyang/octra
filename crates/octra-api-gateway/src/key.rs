//! API key generation.
//!
//! Keys are high-entropy random tokens with an `ok_` prefix. Only the display
//! prefix and the SHA-256 hash are persisted; the plaintext is returned exactly
//! once on creation and never logged.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "ok_";
const KEY_BODY_BYTES: usize = 32;
const DISPLAY_PREFIX_LEN: usize = KEY_PREFIX.len() + 8;

#[derive(Debug, Clone)]
pub struct GeneratedApiKey {
    /// The full plaintext key. Show once, never log or persist.
    pub plaintext: String,
    /// Stable display prefix, e.g. `ok_AbCdEfGh`.
    pub prefix: String,
    /// Hex-encoded SHA-256 of the plaintext key.
    pub hash: String,
}

pub fn generate_api_key() -> GeneratedApiKey {
    let mut bytes = [0u8; KEY_BODY_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let plaintext = format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    let prefix = plaintext
        .chars()
        .take(DISPLAY_PREFIX_LEN)
        .collect::<String>();
    let hash = hex::encode(Sha256::digest(plaintext.as_bytes()));
    GeneratedApiKey {
        plaintext,
        prefix,
        hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_unique_prefixed_and_hashed() {
        let first = generate_api_key();
        let second = generate_api_key();

        assert!(first.plaintext.starts_with("ok_"));
        assert_ne!(first.plaintext, second.plaintext);
        assert!(first.prefix.starts_with("ok_"));
        assert_eq!(first.prefix.len(), DISPLAY_PREFIX_LEN);
        assert!(first.plaintext.starts_with(&first.prefix));
        assert_eq!(first.hash, octra_runtime::hash_api_key(&first.plaintext));
        assert_eq!(first.hash.len(), 64);
        assert!(!first.hash.contains(&first.plaintext));
    }
}
