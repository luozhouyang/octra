-- MCP Gateway extensions: transports, tool enablement, ACLs, API keys, and
-- richer invocation audit rows. Secrets are never stored: header values are
-- references (env:, keyring:, file:) and API keys keep a prefix plus hash.

ALTER TABLE mcp_servers ADD COLUMN config_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE mcp_servers ADD COLUMN headers_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE mcp_servers ADD COLUMN protocol_version TEXT;
ALTER TABLE mcp_servers ADD COLUMN last_seen_at TEXT;

ALTER TABLE mcp_tools ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS mcp_tool_acls (
    id          TEXT PRIMARY KEY,
    tool_id     TEXT NOT NULL REFERENCES mcp_tools(id) ON DELETE CASCADE,
    consumer_id TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    allowed     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (tool_id, consumer_id)
);
CREATE INDEX IF NOT EXISTS idx_mcp_tool_acls_tool ON mcp_tool_acls(tool_id);
CREATE INDEX IF NOT EXISTS idx_mcp_tool_acls_consumer ON mcp_tool_acls(consumer_id);

CREATE TABLE IF NOT EXISTS mcp_api_keys (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    key_prefix  TEXT NOT NULL,
    key_hash    TEXT NOT NULL UNIQUE,
    consumer_id TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    enabled     INTEGER NOT NULL DEFAULT 1,
    expires_at  TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_mcp_api_keys_consumer ON mcp_api_keys(consumer_id);

ALTER TABLE mcp_invocations ADD COLUMN request_id TEXT NOT NULL DEFAULT '';
ALTER TABLE mcp_invocations ADD COLUMN duration_ms INTEGER;
CREATE INDEX IF NOT EXISTS idx_mcp_invocations_tool ON mcp_invocations(tool_id);
