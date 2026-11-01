-- Plugin catalog and per-scope configuration.
-- Built-in plugin manifests are seeded at runtime; artifacts are stored on
-- disk with only path and hash kept in SQLite.

ALTER TABLE plugins ADD COLUMN runtime_type TEXT NOT NULL DEFAULT 'builtin';
ALTER TABLE plugins ADD COLUMN priority INTEGER NOT NULL DEFAULT 100;
ALTER TABLE plugins ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE plugins ADD COLUMN manifest_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE plugins ADD COLUMN config_schema_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE plugins ADD COLUMN artifact_path TEXT;
ALTER TABLE plugins ADD COLUMN artifact_hash TEXT;
ALTER TABLE plugins ADD COLUMN updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

ALTER TABLE plugin_configs ADD COLUMN scope_id TEXT NOT NULL DEFAULT '';
ALTER TABLE plugin_configs ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE plugin_configs ADD COLUMN updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_configs_scope
    ON plugin_configs(plugin_id, scope, scope_id);
CREATE INDEX IF NOT EXISTS idx_plugin_configs_scope_lookup
    ON plugin_configs(scope, scope_id);
