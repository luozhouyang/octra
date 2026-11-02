-- Gateway data entity refactor.
--
-- The project is pre-release, so this migration replaces the original entity
-- tables outright instead of carrying a compatibility layer. No data is
-- backfilled from the previous shapes.

-- Child tables that reference consumers must be rebuilt so their foreign keys
-- point at the new consumers table.
DROP TABLE IF EXISTS mcp_tool_acls;
DROP TABLE IF EXISTS mcp_api_keys;
DROP TABLE IF EXISTS llm_virtual_keys;

-- The plugin catalog (`plugins`) and `plugin_configs` remain in place for the
-- LLM plugin system, which uses its own scope vocabulary.
DROP TABLE IF EXISTS credentials;
DROP TABLE IF EXISTS keyauth_credentials;
DROP TABLE IF EXISTS basicauth_credentials;
DROP TABLE IF EXISTS jwt_secrets;
DROP TABLE IF EXISTS acls;
DROP TABLE IF EXISTS targets;
DROP TABLE IF EXISTS upstreams;
DROP TABLE IF EXISTS routes;
DROP TABLE IF EXISTS services;
DROP TABLE IF EXISTS consumers;

CREATE TABLE services (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    protocol        TEXT NOT NULL DEFAULT 'http',
    host            TEXT NOT NULL,
    port            INTEGER NOT NULL DEFAULT 80,
    path            TEXT,
    retries         INTEGER NOT NULL DEFAULT 5,
    connect_timeout INTEGER NOT NULL DEFAULT 60000,
    write_timeout   INTEGER NOT NULL DEFAULT 60000,
    read_timeout    INTEGER NOT NULL DEFAULT 60000,
    enabled         INTEGER NOT NULL DEFAULT 1,
    description     TEXT NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    updated_at      INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);

CREATE TABLE routes (
    id                 TEXT PRIMARY KEY,
    name               TEXT UNIQUE,
    protocols          TEXT NOT NULL DEFAULT '["http","https"]',
    methods            TEXT,
    hosts              TEXT,
    paths              TEXT,
    headers            TEXT,
    strip_path         INTEGER NOT NULL DEFAULT 1,
    preserve_host      INTEGER NOT NULL DEFAULT 0,
    request_buffering  INTEGER NOT NULL DEFAULT 1,
    response_buffering INTEGER NOT NULL DEFAULT 1,
    regex_priority     INTEGER NOT NULL DEFAULT 0,
    service_id         TEXT REFERENCES services(id) ON DELETE CASCADE,
    enabled            INTEGER NOT NULL DEFAULT 1,
    tags               TEXT NOT NULL DEFAULT '[]',
    created_at         INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    updated_at         INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);
CREATE INDEX idx_routes_service ON routes(service_id);

CREATE TABLE upstreams (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    algorithm    TEXT NOT NULL DEFAULT 'round-robin',
    hash_on      TEXT NOT NULL DEFAULT 'none',
    hash_fallback TEXT NOT NULL DEFAULT 'none',
    slots        INTEGER NOT NULL DEFAULT 10000,
    healthchecks TEXT NOT NULL DEFAULT '{}',
    host_header  TEXT,
    tags         TEXT NOT NULL DEFAULT '[]',
    created_at   INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    updated_at   INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);

CREATE TABLE targets (
    id          TEXT PRIMARY KEY,
    upstream_id TEXT NOT NULL REFERENCES upstreams(id) ON DELETE CASCADE,
    target      TEXT NOT NULL,
    weight      INTEGER NOT NULL DEFAULT 100,
    tags        TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    updated_at  INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    UNIQUE(upstream_id, target)
);
CREATE INDEX idx_targets_upstream ON targets(upstream_id);

CREATE TABLE consumers (
    id         TEXT PRIMARY KEY,
    username   TEXT UNIQUE,
    custom_id  TEXT UNIQUE,
    tags       TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);

CREATE TABLE plugin_instances (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    instance_name TEXT,
    config        TEXT NOT NULL DEFAULT '{}',
    enabled       INTEGER NOT NULL DEFAULT 1,
    protocols     TEXT NOT NULL DEFAULT '["grpc","grpcs","http","https"]',
    ordering      TEXT,
    service_id    TEXT REFERENCES services(id) ON DELETE CASCADE,
    route_id      TEXT REFERENCES routes(id) ON DELETE CASCADE,
    consumer_id   TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    tags          TEXT NOT NULL DEFAULT '[]',
    created_at    INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    updated_at    INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);
CREATE UNIQUE INDEX idx_plugin_instances_scope ON plugin_instances(
    name,
    IFNULL(instance_name, ''),
    IFNULL(service_id, ''),
    IFNULL(route_id, ''),
    IFNULL(consumer_id, '')
);
CREATE INDEX idx_plugin_instances_service ON plugin_instances(service_id);
CREATE INDEX idx_plugin_instances_route ON plugin_instances(route_id);
CREATE INDEX idx_plugin_instances_consumer ON plugin_instances(consumer_id);

CREATE TABLE keyauth_credentials (
    id          TEXT PRIMARY KEY,
    consumer_id TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    key         TEXT NOT NULL UNIQUE,
    ttl         INTEGER,
    tags        TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);
CREATE INDEX idx_keyauth_consumer ON keyauth_credentials(consumer_id);

CREATE TABLE basicauth_credentials (
    id          TEXT PRIMARY KEY,
    consumer_id TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    username    TEXT NOT NULL UNIQUE,
    password    TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER))
);
CREATE INDEX idx_basicauth_consumer ON basicauth_credentials(consumer_id);

CREATE TABLE jwt_secrets (
    id             TEXT PRIMARY KEY,
    consumer_id    TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    key            TEXT NOT NULL,
    secret         TEXT NOT NULL,
    algorithm      TEXT NOT NULL DEFAULT 'HS256',
    rsa_public_key TEXT,
    tags           TEXT NOT NULL DEFAULT '[]',
    created_at     INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    UNIQUE(key, algorithm)
);
CREATE INDEX idx_jwt_consumer ON jwt_secrets(consumer_id);

CREATE TABLE acls (
    id          TEXT PRIMARY KEY,
    consumer_id TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    "group"     TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL DEFAULT (CAST(strftime('%s', 'now') AS INTEGER)),
    UNIQUE(consumer_id, "group")
);
CREATE INDEX idx_acls_consumer ON acls(consumer_id, "group");

-- Rebuild the LLM/MCP tables whose foreign keys referenced the old consumers.
CREATE TABLE llm_virtual_keys (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    key_hash         TEXT NOT NULL UNIQUE,
    key_prefix       TEXT NOT NULL DEFAULT '',
    enabled          INTEGER NOT NULL DEFAULT 1,
    expires_at       TEXT,
    last_used_at     TEXT,
    permissions_json TEXT NOT NULL DEFAULT '{}',
    consumer_id      TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE mcp_tool_acls (
    id          TEXT PRIMARY KEY,
    tool_id     TEXT NOT NULL REFERENCES mcp_tools(id) ON DELETE CASCADE,
    consumer_id TEXT NOT NULL REFERENCES consumers(id) ON DELETE CASCADE,
    allowed     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (tool_id, consumer_id)
);
CREATE INDEX idx_mcp_tool_acls_tool ON mcp_tool_acls(tool_id);
CREATE INDEX idx_mcp_tool_acls_consumer ON mcp_tool_acls(consumer_id);

CREATE TABLE mcp_api_keys (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    key_prefix  TEXT NOT NULL,
    key_hash    TEXT NOT NULL UNIQUE,
    consumer_id TEXT REFERENCES consumers(id) ON DELETE CASCADE,
    enabled     INTEGER NOT NULL DEFAULT 1,
    expires_at  TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_mcp_api_keys_consumer ON mcp_api_keys(consumer_id);
