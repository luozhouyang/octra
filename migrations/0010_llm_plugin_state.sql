-- Host-maintained state for stateful LLM plugins (for example budget
-- windows). Values are opaque JSON owned by the plugin.

CREATE TABLE IF NOT EXISTS llm_plugin_state (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
