//! Master-key management and authenticated encryption for secrets stored in
//! the Octra database.
//!
//! Secrets are sealed with AES-256-GCM under a master key resolved from, in
//! order: the `OCTRA_MASTER_KEY` environment variable, the OS keychain, or a
//! private key file. Sealed values use the `enc:v1:` prefix so legacy
//! plaintext rows remain readable until they are backfilled.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

const ENV_MASTER_KEY: &str = "OCTRA_MASTER_KEY";
const KEYCHAIN_SERVICE: &str = "dev.octra";
const KEYCHAIN_ACCOUNT: &str = "master-key";
const SEALED_PREFIX: &str = "enc:v1:";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// A 256-bit master key used to seal stored secrets.
#[derive(Clone)]
pub struct MasterKey {
    bytes: [u8; KEY_LEN],
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MasterKey([redacted])")
    }
}

impl MasterKey {
    /// Resolves the master key in order: `OCTRA_MASTER_KEY` (base64, exactly
    /// 32 bytes), an existing key file, the OS keychain, then a newly created
    /// keychain entry or key file.
    ///
    /// `allow_create` must be false when the database already exists. That
    /// prevents a missing keychain entry or deleted key file from silently
    /// generating a new key that cannot decrypt the existing rows.
    pub fn load_or_create(
        key_file: &Path,
        use_keychain: bool,
        allow_create: bool,
    ) -> Result<Self, String> {
        if let Some(encoded) = env_master_key()? {
            return Self::from_base64(&encoded).map_err(|_| {
                format!("{ENV_MASTER_KEY} must be base64 encoded and exactly {KEY_LEN} bytes")
            });
        }

        // Prefer an existing file so installations that started before the
        // keychain integration keep the key that encrypted their database.
        if key_file.exists() {
            return load_key_file(key_file);
        }

        if use_keychain && let Some(key) = load_from_keychain()? {
            return Ok(key);
        }

        if !allow_create {
            return Err(format!(
                "master key unavailable: set {ENV_MASTER_KEY}, restore {}, or allow OS keychain access",
                key_file.display()
            ));
        }

        load_or_create_key_file(key_file)
    }

    /// Creates an ephemeral in-memory key for tests and in-memory databases.
    pub fn ephemeral() -> Self {
        Self::generate().expect("system random number generator is unavailable")
    }

    fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; KEY_LEN];
        SystemRandom::new().fill(&mut bytes).map_err(|_| {
            "cannot generate a master key: system random number generator failed".to_string()
        })?;
        Ok(Self { bytes })
    }

    fn from_base64(encoded: &str) -> Result<Self, ()> {
        let decoded = STANDARD.decode(encoded.trim()).map_err(|_| ())?;
        let bytes: [u8; KEY_LEN] = decoded.try_into().map_err(|_| ())?;
        Ok(Self { bytes })
    }
}

/// Seals and opens stored secret values with a master key.
#[derive(Clone)]
pub struct SecretBox {
    key: MasterKey,
}

impl SecretBox {
    pub fn new(key: MasterKey) -> Self {
        Self { key }
    }

    /// Encrypts a plaintext value and returns `enc:v1:<base64>`.
    pub fn seal(&self, plaintext: &[u8]) -> Result<String, String> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| "cannot seal secret: system random number generator failed".to_string())?;

        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &self.key.bytes)
                .map_err(|_| "cannot initialize AES-256-GCM".to_string())?,
        );
        let mut in_out = plaintext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut in_out,
        )
        .map_err(|_| "cannot encrypt secret".to_string())?;

        let mut payload = Vec::with_capacity(NONCE_LEN + in_out.len());
        payload.extend_from_slice(&nonce_bytes);
        payload.extend_from_slice(&in_out);
        Ok(format!("{SEALED_PREFIX}{}", STANDARD.encode(payload)))
    }

    /// Decrypts a sealed value. Values without the `enc:v1:` prefix are legacy
    /// plaintext and are returned unchanged.
    pub fn open(&self, stored: &str) -> Result<Vec<u8>, String> {
        let Some(encoded) = stored.strip_prefix(SEALED_PREFIX) else {
            return Ok(stored.as_bytes().to_vec());
        };
        let payload = STANDARD
            .decode(encoded)
            .map_err(|_| "stored secret is not valid base64".to_string())?;
        if payload.len() < NONCE_LEN + TAG_LEN {
            return Err("stored secret is too short".to_string());
        }
        let (nonce_bytes, ciphertext) = payload.split_at(NONCE_LEN);
        let nonce_bytes: [u8; NONCE_LEN] = nonce_bytes
            .try_into()
            .map_err(|_| "stored secret has an invalid nonce".to_string())?;
        let mut in_out = ciphertext.to_vec();
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &self.key.bytes)
                .map_err(|_| "cannot initialize AES-256-GCM".to_string())?,
        );
        let plaintext = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::empty(),
                &mut in_out,
            )
            .map_err(|_| "stored secret failed authentication".to_string())?;
        Ok(plaintext.to_vec())
    }

    pub fn is_sealed(stored: &str) -> bool {
        stored.starts_with(SEALED_PREFIX)
    }
}

fn env_master_key() -> Result<Option<String>, String> {
    match std::env::var(ENV_MASTER_KEY) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{ENV_MASTER_KEY} is not valid Unicode"))
        }
    }
}

fn load_from_keychain() -> Result<Option<MasterKey>, String> {
    let entry = match keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
        Ok(entry) => entry,
        Err(error) => {
            tracing::warn!(
                %error,
                "OS keychain is unavailable; falling back to the master key file"
            );
            return Ok(None);
        }
    };

    match entry.get_password() {
        Ok(encoded) => MasterKey::from_base64(&encoded).map(Some).map_err(|_| {
            "OS keychain master key is invalid: expected base64 for exactly 32 bytes".to_string()
        }),
        Err(keyring::Error::NoEntry) => {
            let key = match MasterKey::generate() {
                Ok(key) => key,
                Err(error) => {
                    tracing::warn!(%error, "cannot generate an OS keychain master key");
                    return Ok(None);
                }
            };
            let encoded = STANDARD.encode(key.bytes);
            match entry.set_password(&encoded) {
                Ok(()) => Ok(Some(key)),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "cannot store the master key in the OS keychain; falling back to the master key file"
                    );
                    Ok(None)
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "cannot read the master key from the OS keychain; falling back to the master key file"
            );
            Ok(None)
        }
    }
}

fn load_or_create_key_file(path: &Path) -> Result<MasterKey, String> {
    if path.exists() {
        return load_key_file(path);
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "cannot create master key directory {}: {error}",
                parent.display()
            )
        })?;
    }

    let key = MasterKey::generate()?;
    let encoded = STANDARD.encode(key.bytes);
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create master key file {}: {error}", path.display()))?;
    file.write_all(encoded.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("cannot write master key file {}: {error}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!(
                "cannot restrict master key file permissions {}: {error}",
                path.display()
            )
        })?;
    }

    Ok(key)
}

fn load_key_file(path: &Path) -> Result<MasterKey, String> {
    let encoded = fs::read_to_string(path)
        .map_err(|error| format!("cannot read master key file {}: {error}", path.display()))?;
    MasterKey::from_base64(&encoded).map_err(|_| {
        format!(
            "master key file {} is invalid: expected base64 for exactly 32 bytes",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_box() -> SecretBox {
        SecretBox::new(MasterKey::ephemeral())
    }

    #[test]
    fn seal_open_round_trip() {
        let secret = secret_box();
        let sealed = secret.seal(b"correct horse battery staple").unwrap();
        assert!(SecretBox::is_sealed(&sealed));
        assert_eq!(
            secret.open(&sealed).unwrap(),
            b"correct horse battery staple"
        );
    }

    #[test]
    fn random_nonces_produce_different_ciphertexts() {
        let secret = secret_box();
        let first = secret.seal(b"same value").unwrap();
        let second = secret.seal(b"same value").unwrap();
        assert_ne!(first, second);
        assert_eq!(secret.open(&first).unwrap(), secret.open(&second).unwrap());
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let sealed = secret_box().seal(b"private").unwrap();
        let error = SecretBox::new(MasterKey::ephemeral())
            .open(&sealed)
            .unwrap_err();
        assert!(error.contains("authentication"));
    }

    #[test]
    fn legacy_plaintext_is_returned_unchanged() {
        let secret = secret_box();
        assert!(!SecretBox::is_sealed("plain-token"));
        assert_eq!(secret.open("plain-token").unwrap(), b"plain-token");
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private_and_reusable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("master.key");
        let first = MasterKey::load_or_create(&path, false, true).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let second = MasterKey::load_or_create(&path, false, true).unwrap();
        let secret = SecretBox::new(first);
        let sealed = secret.seal(b"persisted").unwrap();
        assert_eq!(SecretBox::new(second).open(&sealed).unwrap(), b"persisted");
    }

    #[test]
    fn key_file_without_keychain_works() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("master.key");
        let key = MasterKey::load_or_create(&path, false, true).unwrap();
        let sealed = SecretBox::new(key).seal(b"no keychain").unwrap();
        assert!(path.exists());
        assert!(sealed.starts_with(SEALED_PREFIX));
    }

    #[test]
    fn missing_key_without_creation_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("master.key");
        let error = MasterKey::load_or_create(&path, false, false).unwrap_err();
        assert!(error.contains("master key unavailable"), "{error}");
        assert!(!path.exists(), "a refused load must not create a key file");
    }

    #[test]
    fn env_key_is_used() {
        if std::env::var_os("OCTRA_SECRETS_ENV_CHILD").is_some() {
            let directory = tempfile::tempdir().unwrap();
            let key_file = directory.path().join("master.key");
            let key = MasterKey::load_or_create(&key_file, true, true).unwrap();
            assert!(!key_file.exists(), "environment key must not create a file");
            let sealed = SecretBox::new(key).seal(b"env secret").unwrap();
            assert!(
                SecretBox::new(MasterKey::ephemeral())
                    .open(&sealed)
                    .unwrap_err()
                    .contains("authentication")
            );
            return;
        }

        let key = [7u8; KEY_LEN];
        let encoded = STANDARD.encode(key);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::env_key_is_used", "--nocapture"])
            .env(ENV_MASTER_KEY, encoded)
            .env("OCTRA_SECRETS_ENV_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
