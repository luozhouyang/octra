-- Octra initial schema.
-- Secrets are never stored here: tables keep secret references only.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS services (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    protocol    TEXT NOT NULL DEFAULT 'http',
    description TEXT NOT NULL DEFAULT '',
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS upstreams (
    id         TEXT PRIMARY KEY,
    service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    algorithm  TEXT NOT NULL DEFAULT 'round_robin',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_upstreams_service ON upstreams(service_id);

CREATE TABLE IF NOT EXISTS targets (
    id          TEXT PRIMARY KEY,
    upstream_id TEXT NOT NULL REFERENCES upstreams(id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    weight      INTEGER NOT NULL DEFAULT 100,
    healthy     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_targets_upstream ON targets(upstream_id);

CREATE TABLE IF NOT EXISTS routes (
    id          TEXT PRIMARY KEY,
    service_id  TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
    host        TEXT,
    path_prefix TEXT NOT NULL DEFAULT '/',
    methods     TEXT NOT NULL DEFAULT '[]',
    headers     TEXT NOT NULL DEFAULT '{}',
    protocol    TEXT NOT NULL DEFAULT 'http',
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_routes_service ON routes(service_id);

CREATE TABLE IF NOT EXISTS consumers (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL DEFAULT 'user',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS credentials (
    id          TEXT PRIMARY KEY,
    consumer_id TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    secret_ref  TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_credentials_consumer ON credentials(consumer_id);

CREATE TABLE IF NOT EXISTS policies (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    scope       TEXT NOT NULL DEFAULT 'global',
    consumer_id TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    document    TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS plugins (
    id      TEXT PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    version TEXT NOT NULL,
    kind    TEXT NOT NULL DEFAULT 'native',
    enabled INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS plugin_configs (
    id         TEXT PRIMARY KEY,
    plugin_id  TEXT NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
    scope      TEXT NOT NULL DEFAULT 'global',
    config     TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_plugin_configs_plugin ON plugin_configs(plugin_id);

CREATE TABLE IF NOT EXISTS llm_providers (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL,
    base_url   TEXT,
    secret_ref TEXT,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS llm_models (
    id           TEXT PRIMARY KEY,
    provider_id  TEXT NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
    model_id     TEXT NOT NULL,
    capabilities TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (provider_id, model_id)
);

CREATE TABLE IF NOT EXISTS llm_routing_groups (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    strategy   TEXT NOT NULL DEFAULT 'priority',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS llm_routes (
    id                TEXT PRIMARY KEY,
    virtual_model_id  TEXT NOT NULL UNIQUE,
    routing_group_id  TEXT REFERENCES llm_routing_groups(id) ON DELETE SET NULL,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS llm_virtual_keys (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    key_hash    TEXT NOT NULL UNIQUE,
    consumer_id TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS llm_usage (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    virtual_model_id TEXT NOT NULL,
    provider_id      TEXT,
    model_id         TEXT,
    input_tokens     INTEGER NOT NULL DEFAULT 0,
    output_tokens    INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_llm_usage_created ON llm_usage(created_at);

CREATE TABLE IF NOT EXISTS agents (
    id              TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL,
    vendor          TEXT,
    detected_version TEXT,
    path            TEXT,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS agent_profiles (
    id         TEXT PRIMARY KEY,
    agent_id   TEXT NOT NULL,
    name       TEXT NOT NULL,
    model_id   TEXT,
    endpoint   TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_agent_profiles_agent ON agent_profiles(agent_id);

CREATE TABLE IF NOT EXISTS agent_config_backups (
    id         TEXT PRIMARY KEY,
    agent_id   TEXT NOT NULL,
    path       TEXT NOT NULL,
    content    TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS mcp_servers (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    transport  TEXT NOT NULL,
    command    TEXT,
    url        TEXT,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS mcp_tools (
    id          TEXT PRIMARY KEY,
    server_id   TEXT NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    schema      TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (server_id, name)
);

CREATE TABLE IF NOT EXISTS mcp_sessions (
    id         TEXT PRIMARY KEY,
    server_id  TEXT NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    status     TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS mcp_invocations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    server_id   TEXT NOT NULL,
    tool_id     TEXT NOT NULL,
    consumer_id TEXT,
    status      TEXT NOT NULL DEFAULT 'ok',
    started_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at TEXT,
    error       TEXT
);
CREATE INDEX IF NOT EXISTS idx_mcp_invocations_started ON mcp_invocations(started_at);

CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS audit_logs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    actor      TEXT,
    action     TEXT NOT NULL,
    target     TEXT,
    detail     TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_audit_logs_created ON audit_logs(created_at);

INSERT OR IGNORE INTO settings (key, value) VALUES ('schema_version', '1');
