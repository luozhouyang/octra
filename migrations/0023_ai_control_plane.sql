-- AI-native control plane: providers, accounts, models, targets, policies,
-- policy attachments, and configuration revisions.

CREATE TABLE ai_providers (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  protocol TEXT NOT NULL,
  base_url TEXT NOT NULL DEFAULT '',
  auth_ref TEXT,
  capabilities_json TEXT NOT NULL DEFAULT '[]',
  config_json TEXT NOT NULL DEFAULT '{}',
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))
);

CREATE TABLE ai_provider_accounts (
  id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL REFERENCES ai_providers(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  credential_kind TEXT NOT NULL DEFAULT 'static',
  secret_ref TEXT,
  oauth_ref TEXT,
  weight INTEGER NOT NULL DEFAULT 1,
  priority INTEGER NOT NULL DEFAULT 0,
  quota_limit INTEGER,
  quota_unit TEXT NOT NULL DEFAULT 'requests',
  quota_period TEXT NOT NULL DEFAULT 'none',
  quota_used INTEGER NOT NULL DEFAULT 0,
  cooldown_until TEXT,
  enabled INTEGER NOT NULL DEFAULT 1,
  config_json TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  UNIQUE (provider_id, name)
);

CREATE TABLE ai_models (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL DEFAULT 'model' CHECK (kind IN ('model', 'group')),
  capabilities_json TEXT NOT NULL DEFAULT '[]',
  formats_json TEXT NOT NULL DEFAULT '[]',
  route_paths_json TEXT NOT NULL DEFAULT '[]',
  balancer_json TEXT NOT NULL DEFAULT '{}',
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))
);

CREATE TABLE ai_model_targets (
  id TEXT PRIMARY KEY,
  model_id TEXT NOT NULL REFERENCES ai_models(id) ON DELETE CASCADE,
  target_model_id TEXT REFERENCES ai_models(id) ON DELETE CASCADE,
  provider_id TEXT REFERENCES ai_providers(id) ON DELETE CASCADE,
  upstream_model TEXT,
  account_id TEXT REFERENCES ai_provider_accounts(id) ON DELETE SET NULL,
  weight INTEGER NOT NULL DEFAULT 1,
  priority INTEGER NOT NULL DEFAULT 0,
  position INTEGER NOT NULL DEFAULT 0,
  config_json TEXT NOT NULL DEFAULT '{}',
  semantic_description TEXT,
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  CHECK (target_model_id IS NOT NULL OR (provider_id IS NOT NULL AND upstream_model IS NOT NULL))
);

CREATE TABLE ai_policies (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  policy_type TEXT NOT NULL,
  config_json TEXT NOT NULL DEFAULT '{}',
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))
);

CREATE TABLE ai_policy_attachments (
  id TEXT PRIMARY KEY,
  policy_id TEXT NOT NULL REFERENCES ai_policies(id) ON DELETE CASCADE,
  scope TEXT NOT NULL,
  scope_id TEXT NOT NULL DEFAULT '',
  order_index INTEGER NOT NULL DEFAULT 0,
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  UNIQUE (policy_id, scope, scope_id)
);

CREATE TABLE ai_config_revisions (
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
  actor TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'applied',
  diff_json TEXT NOT NULL DEFAULT '{}',
  applied_at INTEGER
);

CREATE INDEX idx_ai_provider_accounts_provider ON ai_provider_accounts(provider_id);
CREATE INDEX idx_ai_model_targets_model ON ai_model_targets(model_id);
CREATE INDEX idx_ai_model_targets_target_model ON ai_model_targets(target_model_id);
CREATE INDEX idx_ai_policy_attachments_scope ON ai_policy_attachments(scope, scope_id);
CREATE INDEX idx_ai_policy_attachments_policy ON ai_policy_attachments(policy_id);
CREATE INDEX idx_ai_config_revisions_created_at ON ai_config_revisions(created_at);
