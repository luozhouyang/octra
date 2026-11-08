-- Kong-shaped MCP server modes and aggregation sources.
--
-- `mode` replaces the implicit "one endpoint per registered transport" model
-- with an explicit server type. Existing rows keep the legacy behaviour and are
-- therefore `passthrough-listener`. `sources_json` holds a JSON array of server
-- names; it is only meaningful for `listener` servers.

ALTER TABLE mcp_servers ADD COLUMN mode TEXT NOT NULL DEFAULT 'passthrough-listener';
ALTER TABLE mcp_servers ADD COLUMN sources_json TEXT NOT NULL DEFAULT '[]';

-- Server-default ACLs mirror the per-tool scopes. A tool with its own rows
-- ignores these; otherwise a server rule governs every tool of the server.
CREATE TABLE mcp_server_acls (
    id                TEXT PRIMARY KEY,
    server_id         TEXT NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    consumer_id       TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    consumer_group_id TEXT REFERENCES consumer_groups(id) ON DELETE CASCADE,
    allowed           INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (consumer_id IS NULL OR consumer_group_id IS NULL)
);

CREATE INDEX idx_mcp_server_acls_server ON mcp_server_acls(server_id);
CREATE INDEX idx_mcp_server_acls_consumer ON mcp_server_acls(consumer_id);
CREATE INDEX idx_mcp_server_acls_consumer_group ON mcp_server_acls(consumer_group_id);

-- A server has at most one row per scope; the scope columns are nullable, so
-- the uniqueness rules use partial indexes (same shape as mcp_tool_acls).
CREATE UNIQUE INDEX idx_mcp_server_acls_consumer_scope
    ON mcp_server_acls(server_id, consumer_id)
    WHERE consumer_id IS NOT NULL;
CREATE UNIQUE INDEX idx_mcp_server_acls_group_scope
    ON mcp_server_acls(server_id, consumer_group_id)
    WHERE consumer_group_id IS NOT NULL;
CREATE UNIQUE INDEX idx_mcp_server_acls_default_scope
    ON mcp_server_acls(server_id)
    WHERE consumer_id IS NULL AND consumer_group_id IS NULL;
