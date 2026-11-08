-- Per-server MCP routes and Kong-style CEL allow/deny expressions.
--
-- `route_path` is the explicit public path of a server; NULL means the default
-- `/mcp/<name>` computed by the gateway. Expressions are stored as JSON arrays
-- of CEL source strings and evaluated against the caller, tool, and request.
-- Both columns default to an empty list, which falls back to static ACLs.

ALTER TABLE mcp_servers ADD COLUMN route_path TEXT;
ALTER TABLE mcp_servers ADD COLUMN acl_allow_when_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE mcp_servers ADD COLUMN acl_deny_when_json TEXT NOT NULL DEFAULT '[]';

-- A route path is unique when explicitly configured. NULL rows keep using the
-- name-derived default and are covered by the server name unique constraint.
CREATE UNIQUE INDEX idx_mcp_servers_route_path
    ON mcp_servers(route_path)
    WHERE route_path IS NOT NULL;
