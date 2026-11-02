-- Plugin artifacts and installed WASM plugins.

CREATE TABLE IF NOT EXISTS plugin_artifacts (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    version       TEXT NOT NULL,
    api_version   TEXT NOT NULL,
    hash          TEXT NOT NULL,
    signature     TEXT,
    manifest_json TEXT NOT NULL,
    path          TEXT NOT NULL,
    size_bytes    INTEGER NOT NULL,
    created_at    INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    UNIQUE(name, version)
);

CREATE INDEX IF NOT EXISTS idx_plugin_artifacts_name
    ON plugin_artifacts(name);

ALTER TABLE plugins ADD COLUMN source TEXT NOT NULL DEFAULT 'builtin';
ALTER TABLE plugins ADD COLUMN artifact_id TEXT REFERENCES plugin_artifacts(id);
ALTER TABLE plugins ADD COLUMN storage_path TEXT;
ALTER TABLE plugins ADD COLUMN approved_capabilities_json TEXT NOT NULL DEFAULT '[]';
