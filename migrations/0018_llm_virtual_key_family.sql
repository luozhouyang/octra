-- Virtual key rotation: stable family identity and rotation lineage.

ALTER TABLE llm_virtual_keys ADD COLUMN key_family TEXT;
ALTER TABLE llm_virtual_keys ADD COLUMN rotated_from TEXT;
UPDATE llm_virtual_keys SET key_family = id WHERE key_family IS NULL;
CREATE INDEX IF NOT EXISTS idx_llm_virtual_keys_family
    ON llm_virtual_keys(key_family);
