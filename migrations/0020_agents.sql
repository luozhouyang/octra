-- Agent integration: extend the initial agent tables for configuration
-- hosting.
--
-- `0001_init.sql` reserved `agent_profiles` and `agent_config_backups` with a
-- narrower shape. These statements add the columns the control plane needs
-- without dropping any rows.

ALTER TABLE agent_profiles ADD COLUMN virtual_key_id TEXT;
ALTER TABLE agent_profiles ADD COLUMN config_path TEXT;

-- One profile per (agent, name); the API uses "default" for the active one.
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_profiles_agent_name
    ON agent_profiles(agent_id, name);

ALTER TABLE agent_config_backups ADD COLUMN profile_id TEXT;
ALTER TABLE agent_config_backups ADD COLUMN original_sha256 TEXT NOT NULL DEFAULT '';
ALTER TABLE agent_config_backups ADD COLUMN applied_sha256 TEXT NOT NULL DEFAULT '';
ALTER TABLE agent_config_backups ADD COLUMN file_existed INTEGER NOT NULL DEFAULT 1;
ALTER TABLE agent_config_backups ADD COLUMN restored_at TEXT;

CREATE INDEX IF NOT EXISTS idx_agent_backups_created
    ON agent_config_backups(agent_id, created_at DESC);
