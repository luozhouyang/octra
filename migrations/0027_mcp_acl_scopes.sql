-- MCP ACL scopes: a rule targets exactly one of a consumer, a consumer group,
-- or the tool default (both id columns null). Existing consumer rules are
-- preserved by the copy below.

CREATE TABLE mcp_tool_acls_new (
    id                TEXT PRIMARY KEY,
    tool_id           TEXT NOT NULL REFERENCES mcp_tools(id) ON DELETE CASCADE,
    consumer_id       TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    consumer_group_id TEXT REFERENCES consumer_groups(id) ON DELETE CASCADE,
    allowed           INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (consumer_id IS NULL OR consumer_group_id IS NULL)
);

INSERT INTO mcp_tool_acls_new (id, tool_id, consumer_id, consumer_group_id, allowed, created_at, updated_at)
    SELECT id, tool_id, consumer_id, NULL, allowed, created_at, updated_at
      FROM mcp_tool_acls;

DROP TABLE mcp_tool_acls;
ALTER TABLE mcp_tool_acls_new RENAME TO mcp_tool_acls;

CREATE INDEX idx_mcp_tool_acls_tool ON mcp_tool_acls(tool_id);
CREATE INDEX idx_mcp_tool_acls_consumer ON mcp_tool_acls(consumer_id);
CREATE INDEX idx_mcp_tool_acls_consumer_group ON mcp_tool_acls(consumer_group_id);

-- A tool has at most one row per scope; the scope columns are nullable, so the
-- uniqueness rules use partial indexes.
CREATE UNIQUE INDEX idx_mcp_tool_acls_consumer_scope
    ON mcp_tool_acls(tool_id, consumer_id)
    WHERE consumer_id IS NOT NULL;
CREATE UNIQUE INDEX idx_mcp_tool_acls_group_scope
    ON mcp_tool_acls(tool_id, consumer_group_id)
    WHERE consumer_group_id IS NOT NULL;
CREATE UNIQUE INDEX idx_mcp_tool_acls_default_scope
    ON mcp_tool_acls(tool_id)
    WHERE consumer_id IS NULL AND consumer_group_id IS NULL;
