//! Master-key management and authenticated encryption for secrets stored in
//! the Octra database.
//!
//! Secrets are sealed with AES-256-GCM under the active key of a
//! [`MasterKeySet`]. The set is resolved from, in order: the
//! `OCTRA_MASTER_KEY` environment variable, the OS keychain, or a private key
//! file. Sealed values use the `enc:v2:<key_id>:` prefix and bind the
//! ciphertext to an optional context string (typically `table|row|column`).
//! Legacy `enc:v1:` values and plaintext rows remain readable so existing
//! databases can be upgraded in place.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const ENV_MASTER_KEY: &str = "OCTRA_MASTER_KEY";
const KEYCHAIN_SERVICE: &str = "dev.octra";
const KEYCHAIN_ACTIVE_ACCOUNT: &str = "master-key-active";
// Keychains cannot enumerate entries, so this index lists every retained id
// needed to decrypt ciphertext written with older active keys.
const KEYCHAIN_IDS_ACCOUNT: &str = "master-key-ids";
const KEYCHAIN_LEGACY_ACCOUNT: &str = "master-key";
const KEYCHAIN_KEY_PREFIX: &str = "master-key:";
const ENV_KEY_ID: &str = "env";
const LEGACY_KEY_ID: &str = "legacy";
const SEALED_V1_PREFIX: &str = "enc:v1:";
const SEALED_V2_PREFIX: &str = "enc:v2:";
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
    /// Resolves the active master key in order: `OCTRA_MASTER_KEY` (base64,
    /// exactly 32 bytes), an existing keychain entry, an existing key file,
    /// then a newly created keychain entry or key file.
    ///
    /// `allow_create` must be false when the database already exists. That
    /// prevents a missing keychain entry or deleted key file from silently
    /// generating a new key that cannot decrypt the existing rows.
    pub fn load_or_create(
        key_file: &Path,
        use_keychain: bool,
        allow_create: bool,
    ) -> Result<Self, String> {
        MasterKeySet::load_or_create(key_file, use_keychain, allow_create)?
            .active_key()
            .cloned()
    }

    /// Creates an ephemeral in-memory key for tests and in-memory databases.
    pub fn ephemeral() -> Self {
        Self::generate().expect("system random number generator is unavailable")
    }

    /// Returns the non-secret key id: the first 8 bytes of SHA-256(key) as
    /// lowercase hexadecimal.
    pub fn key_id(&self) -> String {
        let digest = Sha256::digest(self.bytes);
        hex::encode(&digest[..8])
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

    fn encoded(&self) -> String {
        STANDARD.encode(self.bytes)
    }
}

#[derive(Clone)]
enum Persistence {
    Environment,
    Keychain,
    File(PathBuf),
    Memory,
}

/// The active key plus every retained key used to decrypt older ciphertext.
#[derive(Clone)]
pub struct MasterKeySet {
    active: String,
    keys: BTreeMap<String, MasterKey>,
    persistence: Persistence,
}

impl MasterKeySet {
    /// Loads all available keys in order: `OCTRA_MASTER_KEY` (id `env`), the
    /// OS keychain (`master-key-active` plus `master-key:<id>`), then the key
    /// file. Legacy single-key stores are imported under id `legacy`.
    ///
    /// When no key exists and `allow_create` is true, a new key is generated
    /// and persisted to the keychain when available, otherwise to the file.
    pub fn load_or_create(
        key_file: &Path,
        use_keychain: bool,
        allow_create: bool,
    ) -> Result<Self, String> {
        if let Some(encoded) = env_master_key()? {
            let key = MasterKey::from_base64(&encoded).map_err(|_| {
                format!("{ENV_MASTER_KEY} must be base64 encoded and exactly {KEY_LEN} bytes")
            })?;
            return Ok(Self::single(
                ENV_KEY_ID.to_string(),
                key,
                Persistence::Environment,
            ));
        }

        if use_keychain {
            match load_keychain_set() {
                Ok(Some(set)) => return Ok(set),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "OS keychain master key is unavailable; falling back to the master key file"
                    );
                }
            }
        }

        if key_file.exists() {
            return load_key_file_set(key_file);
        }

        if !allow_create {
            return Err(format!(
                "master key unavailable: set {ENV_MASTER_KEY}, restore {}, or allow OS keychain access",
                key_file.display()
            ));
        }

        let key = MasterKey::generate()?;
        let id = key.key_id();
        if use_keychain {
            let set = Self::single(id.clone(), key.clone(), Persistence::Keychain);
            match set.persist() {
                Ok(()) => return Ok(set),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "cannot store the master key in the OS keychain; falling back to the master key file"
                    );
                }
            }
        }

        let set = Self::single(id, key, Persistence::File(key_file.to_path_buf()));
        set.persist()?;
        Ok(set)
    }

    fn single(active: String, key: MasterKey, persistence: Persistence) -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(active.clone(), key);
        Self {
            active,
            keys,
            persistence,
        }
    }

    /// The id of the key used for new ciphertext.
    pub fn active_key_id(&self) -> &str {
        &self.active
    }

    /// Returns every retained key id, in stable sorted order.
    pub fn key_ids(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }

    /// Removes every key that is not the active key and not in `keep`.
    ///
    /// The active key is always retained, so a set can never be emptied. The
    /// remaining key set is persisted before the in-memory set is changed; if
    /// persistence fails, the old keys remain usable in memory.
    pub fn retire(&mut self, keep: &std::collections::HashSet<String>) -> Result<usize, String> {
        let removals: Vec<String> = self
            .keys
            .keys()
            .filter(|id| *id != &self.active && !keep.contains(*id))
            .cloned()
            .collect();
        if removals.is_empty() {
            return Ok(0);
        }
        if matches!(self.persistence, Persistence::Environment) {
            return Err(format!(
                "cannot retire master keys while {ENV_MASTER_KEY} is set: the removal cannot be persisted"
            ));
        }

        let removed: std::collections::HashSet<&String> = removals.iter().collect();
        let remaining: BTreeMap<String, MasterKey> = self
            .keys
            .iter()
            .filter(|(id, _)| !removed.contains(id))
            .map(|(id, key)| (id.clone(), key.clone()))
            .collect();
        if remaining.is_empty() || !remaining.contains_key(&self.active) {
            return Err("cannot retire the last master key".to_string());
        }

        if let Err(error) = self.persist_set(&self.active, &remaining) {
            let _ = self.persist_set(&self.active, &self.keys);
            return Err(error);
        }

        if matches!(self.persistence, Persistence::Keychain) {
            for id in &removals {
                if let Err(error) = delete_keychain_key(id) {
                    let _ = self.persist_set(&self.active, &self.keys);
                    return Err(error);
                }
            }
        }

        self.keys = remaining;
        Ok(removals.len())
    }

    /// Generates and persists a new active key while retaining old keys for
    /// decryption. Returns the new key id.
    ///
    /// Rotation is rejected when the only key source is `OCTRA_MASTER_KEY`,
    /// because there is nowhere to persist the new active key.
    pub fn rotate(&mut self) -> Result<String, String> {
        if matches!(self.persistence, Persistence::Environment) {
            return Err(format!(
                "cannot rotate the master key while {ENV_MASTER_KEY} is set: the new key cannot be persisted"
            ));
        }

        let key = MasterKey::generate()?;
        let id = key.key_id();
        if self.keys.contains_key(&id) {
            return Err("cannot rotate the master key: generated key id collided".to_string());
        }
        self.keys.insert(id.clone(), key);
        let previous_active = std::mem::replace(&mut self.active, id.clone());
        if let Err(error) = self.persist() {
            self.active = previous_active;
            self.keys.remove(&id);
            return Err(error);
        }
        Ok(id)
    }

    fn active_key(&self) -> Result<&MasterKey, String> {
        self.keys
            .get(&self.active)
            .ok_or_else(|| "master key set has no active key".to_string())
    }

    fn persist(&self) -> Result<(), String> {
        self.persist_set(&self.active, &self.keys)
    }

    fn persist_set(&self, active: &str, keys: &BTreeMap<String, MasterKey>) -> Result<(), String> {
        match &self.persistence {
            Persistence::Environment => Err(format!(
                "cannot persist the master key while {ENV_MASTER_KEY} is set"
            )),
            Persistence::Keychain => save_keychain_set(active, keys),
            Persistence::File(path) => write_key_file_set(path, active, keys),
            Persistence::Memory => Ok(()),
        }
    }
}

/// Seals and opens stored secret values with a rotatable master key set.
pub struct SecretBox {
    keys: RwLock<MasterKeySet>,
}

impl SecretBox {
    pub fn new(key: MasterKey) -> Self {
        let id = key.key_id();
        Self::from_key_set(MasterKeySet::single(id, key, Persistence::Memory))
    }

    pub fn from_key_set(key_set: MasterKeySet) -> Self {
        Self {
            keys: RwLock::new(key_set),
        }
    }

    /// Encrypts a plaintext value with an empty AAD context.
    pub fn seal(&self, plaintext: &[u8]) -> Result<String, String> {
        self.seal_with_context(plaintext, "")
    }

    /// Encrypts a plaintext value and binds it to `context` as AES-GCM AAD.
    pub fn seal_with_context(&self, plaintext: &[u8], context: &str) -> Result<String, String> {
        let keys = self
            .keys
            .read()
            .map_err(|_| "master key set lock is poisoned".to_string())?;
        let key = keys.active_key()?;
        let payload = seal_with_key(key, plaintext, context.as_bytes())?;
        Ok(format!(
            "{SEALED_V2_PREFIX}{}:{}",
            keys.active_key_id(),
            STANDARD.encode(payload)
        ))
    }

    /// Decrypts a sealed value with an empty AAD context.
    pub fn open(&self, stored: &str) -> Result<Vec<u8>, String> {
        self.open_with_context(stored, "")
    }

    /// Decrypts a sealed value. v2 values require the exact context used at
    /// sealing time; v1 values are opened without AAD for compatibility.
    /// Values without a sealed prefix are legacy plaintext.
    pub fn open_with_context(&self, stored: &str, context: &str) -> Result<Vec<u8>, String> {
        if let Some(rest) = stored.strip_prefix(SEALED_V2_PREFIX) {
            let (key_id, encoded) = rest
                .split_once(':')
                .ok_or_else(|| "stored secret has an invalid enc:v2: prefix".to_string())?;
            let payload = decode_payload(encoded)?;
            let keys = self
                .keys
                .read()
                .map_err(|_| "master key set lock is poisoned".to_string())?;
            let key = keys.keys.get(key_id).ok_or_else(|| {
                format!("stored secret references unknown master key id '{key_id}'")
            })?;
            return open_with_key(key, &payload, context.as_bytes());
        }

        if let Some(encoded) = stored.strip_prefix(SEALED_V1_PREFIX) {
            let payload = decode_payload(encoded)?;
            let keys = self
                .keys
                .read()
                .map_err(|_| "master key set lock is poisoned".to_string())?;
            for key in keys.keys.values() {
                if let Ok(plaintext) = open_with_key(key, &payload, &[]) {
                    return Ok(plaintext);
                }
            }
            return Err("stored secret failed authentication".to_string());
        }

        Ok(stored.as_bytes().to_vec())
    }

    /// Rotates the active key in place. Old keys stay available for
    /// decryption, so callers can re-encrypt stored rows after this succeeds.
    pub fn rotate(&self) -> Result<String, String> {
        let mut keys = self
            .keys
            .write()
            .map_err(|_| "master key set lock is poisoned".to_string())?;
        keys.rotate()
    }

    /// Returns every retained key id, in stable sorted order.
    pub fn key_ids(&self) -> Result<Vec<String>, String> {
        let keys = self
            .keys
            .read()
            .map_err(|_| "master key set lock is poisoned".to_string())?;
        Ok(keys.key_ids())
    }

    /// Returns the id of the key used for new ciphertext.
    pub fn active_key_id(&self) -> Result<String, String> {
        let keys = self
            .keys
            .read()
            .map_err(|_| "master key set lock is poisoned".to_string())?;
        Ok(keys.active_key_id().to_string())
    }

    /// Removes every non-active key that is not in `keep`.
    pub fn retire(&self, keep: &std::collections::HashSet<String>) -> Result<usize, String> {
        let mut keys = self
            .keys
            .write()
            .map_err(|_| "master key set lock is poisoned".to_string())?;
        keys.retire(keep)
    }

    pub fn is_sealed(stored: &str) -> bool {
        stored.starts_with(SEALED_V1_PREFIX) || stored.starts_with(SEALED_V2_PREFIX)
    }

    /// Writes an `enc:v1:` value with the active key. This exists only to
    /// exercise backward compatibility and must not be used for new data.
    #[doc(hidden)]
    pub fn seal_legacy(&self, plaintext: &[u8]) -> Result<String, String> {
        let keys = self
            .keys
            .read()
            .map_err(|_| "master key set lock is poisoned".to_string())?;
        let key = keys.active_key()?;
        let payload = seal_with_key(key, plaintext, &[])?;
        Ok(format!("{SEALED_V1_PREFIX}{}", STANDARD.encode(payload)))
    }
}

fn seal_with_key(key: &MasterKey, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| "cannot seal secret: system random number generator failed".to_string())?;

    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &key.bytes)
            .map_err(|_| "cannot initialize AES-256-GCM".to_string())?,
    );
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::from(aad),
        &mut in_out,
    )
    .map_err(|_| "cannot encrypt secret".to_string())?;

    let mut payload = Vec::with_capacity(NONCE_LEN + in_out.len());
    payload.extend_from_slice(&nonce_bytes);
    payload.extend_from_slice(&in_out);
    Ok(payload)
}

fn open_with_key(key: &MasterKey, payload: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() < NONCE_LEN + TAG_LEN {
        return Err("stored secret is too short".to_string());
    }
    let (nonce_bytes, ciphertext) = payload.split_at(NONCE_LEN);
    let nonce_bytes: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .map_err(|_| "stored secret has an invalid nonce".to_string())?;
    let mut in_out = ciphertext.to_vec();
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &key.bytes)
            .map_err(|_| "cannot initialize AES-256-GCM".to_string())?,
    );
    let plaintext = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(aad),
            &mut in_out,
        )
        .map_err(|_| "stored secret failed authentication".to_string())?;
    Ok(plaintext.to_vec())
}

fn decode_payload(encoded: &str) -> Result<Vec<u8>, String> {
    STANDARD
        .decode(encoded)
        .map_err(|_| "stored secret is not valid base64".to_string())
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

fn keychain_entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|error| format!("cannot open OS keychain entry '{account}': {error}"))
}

fn read_keychain(account: &str) -> Result<Option<String>, String> {
    match keychain_entry(account)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!(
            "cannot read OS keychain entry '{account}': {error}"
        )),
    }
}

fn delete_keychain_key(id: &str) -> Result<(), String> {
    let account = format!("{KEYCHAIN_KEY_PREFIX}{id}");
    match keychain_entry(&account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "cannot delete OS keychain master key '{id}': {error}"
        )),
    }
}

fn save_keychain_set(active: &str, keys: &BTreeMap<String, MasterKey>) -> Result<(), String> {
    for (id, key) in keys {
        keychain_entry(&format!("{KEYCHAIN_KEY_PREFIX}{id}"))?
            .set_password(&key.encoded())
            .map_err(|error| {
                format!("cannot store master key '{id}' in the OS keychain: {error}")
            })?;
    }
    let ids: Vec<&str> = keys.keys().map(String::as_str).collect();
    let index = serde_json::to_string(&ids)
        .map_err(|error| format!("cannot encode the OS keychain key index: {error}"))?;
    keychain_entry(KEYCHAIN_IDS_ACCOUNT)?
        .set_password(&index)
        .map_err(|error| format!("cannot store the OS keychain key index: {error}"))?;
    keychain_entry(KEYCHAIN_ACTIVE_ACCOUNT)?
        .set_password(active)
        .map_err(|error| {
            format!("cannot store the active master key id in the OS keychain: {error}")
        })
}

fn load_keychain_set() -> Result<Option<MasterKeySet>, String> {
    let Some(active_id) = read_keychain(KEYCHAIN_ACTIVE_ACCOUNT)? else {
        let Some(encoded) = read_keychain(KEYCHAIN_LEGACY_ACCOUNT)? else {
            return Ok(None);
        };
        let key = MasterKey::from_base64(&encoded).map_err(|_| {
            "OS keychain master key is invalid: expected base64 for exactly 32 bytes".to_string()
        })?;
        return Ok(Some(MasterKeySet::single(
            LEGACY_KEY_ID.to_string(),
            key,
            Persistence::Keychain,
        )));
    };

    let active_id = active_id.trim().to_string();
    if active_id.is_empty() {
        return Err("OS keychain active master key id is empty".to_string());
    }
    let mut ids = vec![active_id.clone()];
    if let Some(raw) = read_keychain(KEYCHAIN_IDS_ACCOUNT)? {
        let indexed: Vec<String> = serde_json::from_str(&raw)
            .map_err(|_| "OS keychain master key index is invalid".to_string())?;
        for id in indexed {
            if !ids.iter().any(|existing| existing == &id) {
                ids.push(id);
            }
        }
    }

    let mut keys = BTreeMap::new();
    for id in ids {
        match read_keychain(&format!("{KEYCHAIN_KEY_PREFIX}{id}"))? {
            Some(encoded) => {
                let key = MasterKey::from_base64(&encoded).map_err(|_| {
                    format!(
                        "OS keychain master key '{id}' is invalid: expected base64 for exactly {KEY_LEN} bytes"
                    )
                })?;
                keys.insert(id, key);
            }
            None if id == active_id => {
                return Err(format!(
                    "OS keychain active master key '{active_id}' is missing"
                ));
            }
            None => {
                tracing::warn!(
                    key_id = %id,
                    "ignoring a missing non-active master key in the OS keychain"
                );
            }
        }
    }

    let set = MasterKeySet {
        active: active_id,
        keys,
        persistence: Persistence::Keychain,
    };
    set.active_key()?;
    Ok(Some(set))
}

fn load_key_file_set(path: &Path) -> Result<MasterKeySet, String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("cannot read master key file {}: {error}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("master key file {} is empty", path.display()));
    }

    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(trimmed)
        && (object.contains_key("active") || object.contains_key("keys"))
    {
        let active = object
            .get("active")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "master key file {} is invalid: expected an 'active' key id",
                    path.display()
                )
            })?
            .to_string();
        let encoded_keys = object
            .get("keys")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                format!(
                    "master key file {} is invalid: expected a 'keys' object",
                    path.display()
                )
            })?;
        if encoded_keys.is_empty() {
            return Err(format!(
                "master key file {} is invalid: 'keys' is empty",
                path.display()
            ));
        }
        let mut keys = BTreeMap::new();
        for (id, value) in encoded_keys {
            let encoded = value.as_str().ok_or_else(|| {
                format!(
                    "master key file {} is invalid: key '{id}' must be base64 text",
                    path.display()
                )
            })?;
            let key = MasterKey::from_base64(encoded).map_err(|_| {
                format!(
                    "master key file {} is invalid: key '{id}' must be base64 for exactly {KEY_LEN} bytes",
                    path.display()
                )
            })?;
            keys.insert(id.clone(), key);
        }
        let set = MasterKeySet {
            active,
            keys,
            persistence: Persistence::File(path.to_path_buf()),
        };
        set.active_key()?;
        return Ok(set);
    }

    let key = MasterKey::from_base64(trimmed).map_err(|_| {
        format!(
            "master key file {} is invalid: expected base64 for exactly {KEY_LEN} bytes or JSON with active and keys",
            path.display()
        )
    })?;
    Ok(MasterKeySet::single(
        LEGACY_KEY_ID.to_string(),
        key,
        Persistence::File(path.to_path_buf()),
    ))
}

fn write_key_file_set(
    path: &Path,
    active: &str,
    keys: &BTreeMap<String, MasterKey>,
) -> Result<(), String> {
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

    let mut encoded_keys = Map::new();
    for (id, key) in keys {
        encoded_keys.insert(id.clone(), Value::String(key.encoded()));
    }
    let document = json!({ "active": active, "keys": encoded_keys });
    let bytes = serde_json::to_vec(&document)
        .map_err(|error| format!("cannot encode master key file {}: {error}", path.display()))?;

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
    file.write_all(&bytes)
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn secret_box() -> SecretBox {
        SecretBox::new(MasterKey::ephemeral())
    }

    #[test]
    fn seal_open_round_trip() {
        let secret = secret_box();
        let sealed = secret.seal(b"correct horse battery staple").unwrap();
        assert!(SecretBox::is_sealed(&sealed));
        assert!(sealed.starts_with(SEALED_V2_PREFIX));
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
    fn key_ids_are_short_lowercase_hex() {
        let id = MasterKey::ephemeral().key_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(id, id.to_lowercase());
    }

    #[test]
    fn context_binds_ciphertext_to_aad() {
        let secret = secret_box();
        let sealed = secret
            .seal_with_context(b"private", "keyauth_credentials|row-1|key")
            .unwrap();
        assert!(sealed.starts_with(SEALED_V2_PREFIX));
        assert_eq!(
            secret
                .open_with_context(&sealed, "keyauth_credentials|row-1|key")
                .unwrap(),
            b"private"
        );
        let error = secret
            .open_with_context(&sealed, "keyauth_credentials|row-2|key")
            .unwrap_err();
        assert!(error.contains("authentication"));
    }

    #[test]
    fn unknown_key_id_is_reported_without_key_material() {
        let sealed = secret_box().seal(b"private").unwrap();
        let (_, encoded) = sealed
            .strip_prefix(SEALED_V2_PREFIX)
            .unwrap()
            .split_once(':')
            .unwrap();
        let forged = format!("{SEALED_V2_PREFIX}deadbeefdeadbeef:{encoded}");
        let error = secret_box().open(&forged).unwrap_err();
        assert!(error.contains("deadbeefdeadbeef"));
        assert!(!error.contains(encoded));
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let first = secret_box();
        let sealed = first.seal(b"private").unwrap();
        let (active, key_id) = {
            let keys = first.keys.read().unwrap();
            (keys.active.clone(), keys.active_key().unwrap().key_id())
        };
        let set = MasterKeySet::single(active, MasterKey::ephemeral(), Persistence::Memory);
        assert_eq!(set.active_key_id(), key_id);
        let error = SecretBox::from_key_set(set).open(&sealed).unwrap_err();
        assert!(error.contains("authentication"));
    }

    #[test]
    fn legacy_v1_values_open_after_rotation() {
        let mut set = MasterKeySet::load_or_create(
            &tempfile::tempdir().unwrap().path().join("master.key"),
            false,
            true,
        )
        .unwrap();
        let secret = SecretBox::from_key_set(set.clone());
        let legacy = secret.seal_legacy(b"legacy").unwrap();
        assert!(legacy.starts_with(SEALED_V1_PREFIX));
        assert_eq!(secret.open(&legacy).unwrap(), b"legacy");

        set.rotate().unwrap();
        let rotated = SecretBox::from_key_set(set);
        assert_eq!(rotated.open(&legacy).unwrap(), b"legacy");
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
    fn rotation_persists_all_keys_in_the_key_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("master.key");
        let mut set = MasterKeySet::load_or_create(&path, false, true).unwrap();
        let old_id = set.active_key_id().to_string();
        let old = SecretBox::from_key_set(set.clone())
            .seal_with_context(b"old", "table|row|column")
            .unwrap();
        assert!(old.starts_with(&format!("{SEALED_V2_PREFIX}{old_id}:")));

        let new_id = set.rotate().unwrap();
        assert_ne!(old_id, new_id);
        let current = SecretBox::from_key_set(set.clone())
            .seal_with_context(b"new", "table|row|column")
            .unwrap();
        assert!(current.starts_with(&format!("{SEALED_V2_PREFIX}{new_id}:")));

        let reloaded = MasterKeySet::load_or_create(&path, false, false).unwrap();
        assert_eq!(reloaded.active_key_id(), new_id);
        let reopened = SecretBox::from_key_set(reloaded);
        assert_eq!(
            reopened
                .open_with_context(&old, "table|row|column")
                .unwrap(),
            b"old"
        );
        assert_eq!(
            reopened
                .open_with_context(&current, "table|row|column")
                .unwrap(),
            b"new"
        );
    }

    #[test]
    fn retire_removes_unreferenced_keys_from_the_key_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("master.key");
        let mut set = MasterKeySet::load_or_create(&path, false, true).unwrap();
        let old_id = set.active_key_id().to_string();
        let old_sealed = SecretBox::from_key_set(set.clone())
            .seal(b"retired")
            .unwrap();
        assert!(old_sealed.starts_with(&format!("{SEALED_V2_PREFIX}{old_id}:")));

        let new_id = set.rotate().unwrap();
        let keep = HashSet::from([new_id.clone()]);
        assert_eq!(set.retire(&keep).unwrap(), 1);
        assert_eq!(set.key_ids(), vec![new_id.clone()]);

        let error = SecretBox::from_key_set(set.clone())
            .open(&old_sealed)
            .unwrap_err();
        assert!(error.contains(&old_id), "{error}");

        let reloaded = MasterKeySet::load_or_create(&path, false, false).unwrap();
        assert_eq!(reloaded.active_key_id(), new_id);
        assert_eq!(reloaded.key_ids(), vec![new_id]);
    }

    #[test]
    fn retire_always_keeps_the_active_key() {
        let first = MasterKey::ephemeral();
        let mut set = MasterKeySet::single(first.key_id(), first, Persistence::Memory);
        let new_id = set.rotate().unwrap();

        assert_eq!(set.retire(&HashSet::new()).unwrap(), 1);
        assert_eq!(set.key_ids(), vec![new_id]);
    }

    #[test]
    fn retire_keeps_referenced_keys() {
        let first = MasterKey::ephemeral();
        let mut set = MasterKeySet::single(first.key_id(), first, Persistence::Memory);
        let old_id = set.active_key_id().to_string();
        let new_id = set.rotate().unwrap();

        let keep = HashSet::from([old_id.clone()]);
        assert_eq!(set.retire(&keep).unwrap(), 0);
        let ids = set.key_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&old_id));
        assert!(ids.contains(&new_id));
    }

    #[test]
    fn secret_box_exposes_key_ids_and_retires_in_memory() {
        let first = MasterKey::ephemeral();
        let first_id = first.key_id();
        let secret = SecretBox::new(first);
        assert_eq!(secret.key_ids().unwrap(), vec![first_id]);
        assert_eq!(
            secret.active_key_id().unwrap(),
            secret.key_ids().unwrap()[0]
        );

        let new_id = secret.rotate().unwrap();
        assert_eq!(secret.retire(&HashSet::new()).unwrap(), 1);
        assert_eq!(secret.key_ids().unwrap(), vec![new_id]);
    }

    #[test]
    fn environment_key_store_refuses_retirement_when_a_key_would_be_removed() {
        let active_key = MasterKey::ephemeral();
        let active = active_key.key_id();
        let old_key = MasterKey::ephemeral();
        let old = old_key.key_id();
        let mut keys = BTreeMap::new();
        keys.insert(active.clone(), active_key);
        keys.insert(old.clone(), old_key);
        let mut set = MasterKeySet {
            active,
            keys,
            persistence: Persistence::Environment,
        };

        let keep = HashSet::from([set.active_key_id().to_string()]);
        let error = set.retire(&keep).unwrap_err();
        assert!(error.contains(ENV_MASTER_KEY), "{error}");
        let ids = set.key_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&old));
    }

    #[test]
    fn environment_key_store_allows_a_no_op_retirement() {
        let key = MasterKey::ephemeral();
        let id = key.key_id();
        let mut set = MasterKeySet::single(id.clone(), key, Persistence::Environment);

        assert_eq!(set.retire(&HashSet::new()).unwrap(), 0);
        assert_eq!(set.key_ids(), vec![id]);
    }

    #[test]
    fn legacy_key_file_loads_as_legacy_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("master.key");
        fs::write(&path, format!("{}\n", STANDARD.encode([9u8; KEY_LEN]))).unwrap();

        let set = MasterKeySet::load_or_create(&path, false, false).unwrap();
        assert_eq!(set.active_key_id(), LEGACY_KEY_ID);
        let sealed = SecretBox::from_key_set(set).seal(b"legacy-file").unwrap();
        assert!(sealed.starts_with(&format!("{SEALED_V2_PREFIX}{LEGACY_KEY_ID}:")));
    }

    #[test]
    fn key_file_without_keychain_works() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("master.key");
        let key = MasterKey::load_or_create(&path, false, true).unwrap();
        let sealed = SecretBox::new(key).seal(b"no keychain").unwrap();
        assert!(path.exists());
        assert!(sealed.starts_with(SEALED_V2_PREFIX));
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
    fn env_key_is_used_and_cannot_rotate() {
        if std::env::var_os("OCTRA_SECRETS_ENV_CHILD").is_some() {
            let directory = tempfile::tempdir().unwrap();
            let key_file = directory.path().join("master.key");
            let mut set = MasterKeySet::load_or_create(&key_file, true, true).unwrap();
            assert_eq!(set.active_key_id(), ENV_KEY_ID);
            let error = set.rotate().unwrap_err();
            assert!(error.contains(ENV_MASTER_KEY), "{error}");
            assert!(!key_file.exists(), "environment key must not create a file");
            let sealed = SecretBox::from_key_set(set).seal(b"env secret").unwrap();
            assert!(sealed.starts_with(&format!("{SEALED_V2_PREFIX}{ENV_KEY_ID}:")));
            return;
        }

        let key = [7u8; KEY_LEN];
        let encoded = STANDARD.encode(key);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::env_key_is_used_and_cannot_rotate",
                "--nocapture",
            ])
            .env(ENV_MASTER_KEY, encoded)
            .env("OCTRA_SECRETS_ENV_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
