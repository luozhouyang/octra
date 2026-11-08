-- Deterministic KeyAuth lookup for race-free uniqueness.
--
-- KeyAuth keys are sealed with a random nonce, so the ciphertext UNIQUE
-- constraint cannot detect duplicate plaintext keys. `key_lookup` stores
-- lowercase hex SHA-256(plaintext key); the partial unique index enforces
-- uniqueness without decrypting every row. This is a lookup/dedup hash, not
-- encryption: KeyAuth keys must be high-entropy because weak keys remain
-- guessable offline.

ALTER TABLE keyauth_credentials ADD COLUMN key_lookup TEXT;
CREATE UNIQUE INDEX idx_keyauth_credentials_lookup
    ON keyauth_credentials(key_lookup) WHERE key_lookup IS NOT NULL;
