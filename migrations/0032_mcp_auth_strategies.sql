-- MCP data-plane auth strategies.
--
-- A strategy is a named authentication configuration (currently only OIDC)
-- that a server may select explicitly. At most one enabled strategy is the
-- default for the global `/mcp` endpoint. `config_json` holds the kind-specific
-- settings; OIDC stores issuer, JWKS URI, audience, scopes, and claim names.

CREATE TABLE mcp_auth_strategies (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    kind         TEXT NOT NULL,
    config_json  TEXT NOT NULL DEFAULT '{}',
    enabled      INTEGER NOT NULL DEFAULT 1,
    is_default   INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE UNIQUE INDEX idx_mcp_auth_strategies_default
    ON mcp_auth_strategies(is_default) WHERE is_default = 1;

-- A server references at most one strategy. Deleting the strategy clears the
-- reference so the server follows the default strategy and then API keys.
ALTER TABLE mcp_servers ADD COLUMN auth_strategy_id TEXT
    REFERENCES mcp_auth_strategies(id) ON DELETE SET NULL;
