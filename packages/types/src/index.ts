export interface SnapshotCounts {
  services: number;
  routes: number;
  upstreams: number;
  targets: number;
}

export interface SnapshotInfo {
  generated_at: string;
  counts: SnapshotCounts;
}

export interface HealthResponse {
  status: "ok" | "degraded";
  version: string;
  uptime_secs: number;
  storage: "ok" | "error";
  snapshot: SnapshotInfo;
}

export interface ServiceSummary {
  id: string;
  name: string;
  protocol: string;
  enabled: boolean;
}

export interface RouteSummary {
  id: string;
  service_id: string;
  host: string | null;
  path_prefix: string;
  enabled: boolean;
}

export interface ControlConfig {
  daemon: {
    control_bind: string;
  };
  gateway: {
    listen_bind: string;
  };
  storage: {
    path: string;
  };
  logging: {
    level: string;
    format: "pretty" | "json";
  };
}

export type PiiDownloadState =
  | { state: "idle" }
  | { state: "downloading"; downloaded: number; total: number }
  | { state: "verifying" }
  | { state: "ready" }
  | { state: "failed"; message: string }
  | { state: "cancelled" };

export interface PiiModelStatus {
  id: string;
  display_name: string;
  license: string;
  revision: string;
  size: number;
  installed: boolean;
  state: PiiDownloadState;
  last_error?: string;
  default_threshold: number;
  floor: number;
  enabled: boolean;
  threshold: number;
  record_id?: string;
  engine_readiness?: string;
  engine_error?: string;
}

export type PiiModelAction =
  { do: "on" | "off" | "download" | "cancel" | "remove" | "pick" } | { do: "score"; score: number };

// ---------------------------------------------------------------------------
// Control-plane entities

export interface ListResponse<T> {
  data: T[];
  next: string | null;
  offset: string | null;
}

export interface Service {
  id: string;
  name: string;
  protocol: string;
  host: string;
  port: number;
  path: string | null;
  retries: number;
  connect_timeout: number;
  write_timeout: number;
  read_timeout: number;
  enabled: boolean;
  description: string;
  tags: string[];
  created_at: number;
  updated_at: number;
}

export interface RouteView {
  id: string;
  name: string | null;
  protocols: string[];
  methods: string[];
  hosts: string[];
  paths: string[];
  headers: Record<string, string[]>;
  strip_path: boolean;
  preserve_host: boolean;
  request_buffering: boolean;
  response_buffering: boolean;
  regex_priority: number;
  service_id: string | null;
  enabled: boolean;
  tags: string[];
  created_at: number;
  updated_at: number;
  service: { id: string } | null;
}

export interface Upstream {
  id: string;
  name: string;
  algorithm: string;
  hash_on: string;
  hash_fallback: string;
  hash_on_header: string;
  hash_fallback_header: string;
  hash_on_cookie: string;
  hash_on_cookie_path: string;
  hash_on_query_arg: string;
  hash_fallback_query_arg: string;
  hash_on_uri_capture: string;
  hash_fallback_uri_capture: string;
  slots: number;
  healthchecks: unknown;
  host_header: string | null;
  tags: string[];
  created_at: number;
  updated_at: number;
}

export interface TargetView {
  id: string;
  upstream_id: string;
  target: string;
  weight: number;
  tags: string[];
  created_at: number;
  updated_at: number;
  upstream: { id: string };
}

export interface ApiMetricsSnapshot {
  uptime_secs: number;
  requests_total: number;
  status_2xx: number;
  status_3xx: number;
  status_4xx: number;
  status_5xx: number;
  upstream_errors: number;
  retries: number;
  latency_avg_ms: number;
}

export interface RequestLog {
  id: number;
  request_id: string;
  method: string;
  path: string;
  status: number;
  latency_ms: number;
  route_id: string | null;
  service_id: string | null;
  target_url: string | null;
  consumer_id: string | null;
  error: string | null;
  created_at: string;
}

export interface PluginInstanceView {
  id: string;
  name: string;
  instance_name: string | null;
  config: unknown;
  enabled: boolean;
  protocols: string[];
  ordering: unknown | null;
  service_id: string | null;
  route_id: string | null;
  consumer_id: string | null;
  consumer_group_id: string | null;
  tags: string[];
  created_at: number;
  updated_at: number;
  service: { id: string } | null;
  route: { id: string } | null;
  consumer: { id: string } | null;
  consumer_group: { id: string } | null;
}

export interface ApiPluginStats {
  calls: number;
  rejections: number;
  failures: number;
  timeouts: number;
  total_latency_micros: number;
  max_latency_micros: number;
}

// ---------------------------------------------------------------------------
// LLM gateway

export interface LlmVirtualKey {
  id: string;
  name: string;
  key_prefix: string;
  consumer_id: string | null;
  enabled: boolean;
  expires_at: string | null;
  last_used_at: string | null;
  permissions: unknown;
  key_family: string;
  rotated_from: string | null;
  created_at: string;
}

export interface CreatedLlmVirtualKey extends LlmVirtualKey {
  /** Plaintext key, returned exactly once. */
  key_value: string;
  warning: string;
}

export interface LlmUsageTotals {
  requests: number;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  cost_micros: number;
}

export interface LlmUsageModelRow extends LlmUsageTotals {
  virtual_model_id: string;
}

export interface LlmUsageProviderRow extends LlmUsageTotals {
  provider_id: string;
}

export interface LlmUsageRecord {
  id: number;
  request_id: string;
  virtual_model_id: string;
  provider_id: string | null;
  model_id: string | null;
  account_id: string | null;
  account_name: string | null;
  api_key_id: string | null;
  protocol: string | null;
  status_code: number | null;
  latency_ms: number | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  reasoning_tokens: number | null;
  cost_micros: number | null;
  cost_currency: string | null;
  finish_reason: string | null;
  error: string | null;
  created_at: string;
}

export interface LlmUsage {
  totals: LlmUsageTotals;
  by_model: LlmUsageModelRow[];
  by_provider: LlmUsageProviderRow[];
  recent: LlmUsageRecord[];
}

export interface LlmPlugin {
  id: string;
  name: string;
  version: string;
  enabled: boolean;
  priority: number;
  description: string;
  updated_at: string;
}

export type LlmPluginScope = "global" | "consumer" | "virtual_key" | "model" | "provider";

export interface LlmStageMetrics {
  calls: number;
  failures: number;
  timeouts: number;
  rejections: number;
  fail_open: number;
  total_micros: number;
  max_micros: number;
  latency_p50_micros: number;
  latency_p95_micros: number;
  latency_p99_micros: number;
}

export type LlmStageName =
  "request" | "provider_request" | "response" | "final_response" | "stream_event" | "error" | "log";

export interface LlmPluginMetrics {
  name: string;
  config_id: string;
  scope: LlmPluginScope;
  scope_id: string;
  stages: Record<LlmStageName, LlmStageMetrics>;
}

// ---------------------------------------------------------------------------
// Consumer policies

export interface ConsumerPolicy {
  id: string;
  name: string;
  scope: string;
  consumer_id: string | null;
  document: unknown;
  created_at: string;
  updated_at: string;
}

// ---------------------------------------------------------------------------
// MCP gateway

export interface McpServer {
  id: string;
  name: string;
  transport: "stdio" | "http";
  command: string | null;
  url: string | null;
  enabled: boolean;
  config: unknown;
  headers: Record<string, unknown>;
  protocol_version: string | null;
  last_seen_at: string | null;
  created_at: string;
  updated_at: string;
  tool_count: number;
}

export interface McpTool {
  id: string;
  server_id: string;
  name: string;
  description: string;
  schema: unknown;
  enabled: boolean;
  created_at: string;
  server_name: string;
  qualified_name: string;
}

export interface McpSession {
  id: string;
  server_id: string;
  status: string;
  created_at: string;
  updated_at: string;
}

export interface McpInvocation {
  id: number;
  server_id: string;
  tool_id: string;
  consumer_id: string | null;
  request_id: string;
  status: string;
  duration_ms: number;
  started_at: string;
  finished_at: string | null;
  error: string | null;
}

export interface McpApiKey {
  id: string;
  name: string;
  key_prefix: string;
  consumer_id: string | null;
  enabled: boolean;
  expires_at: string | null;
  created_at: string;
}

export interface CreatedMcpApiKey extends McpApiKey {
  key_value: string;
}

export interface McpAcl {
  id: string;
  tool_id: string;
  consumer_id: string;
  allowed: boolean;
  created_at: string;
  updated_at: string;
}

// ---------------------------------------------------------------------------
// Cross-gateway overview

export type OverviewRange = "1h" | "24h" | "7d";

export interface OverviewDestination {
  id: string;
  label: string;
  count: number;
}

export interface Overview {
  range: OverviewRange;
  generated_at: string;
  buckets: number[];
  series: {
    api: number[];
    llm: number[];
    mcp: number[];
    errors: number[];
  };
  totals: {
    api: number;
    llm: number;
    mcp: number;
    errors: number;
    requests: number;
  };
  latency: {
    api_avg_ms: number | null;
    llm_avg_ms: number | null;
    mcp_avg_ms: number | null;
  };
  destinations: {
    api: OverviewDestination[];
    llm: OverviewDestination[];
    mcp: OverviewDestination[];
  };
}

// ---------------------------------------------------------------------------
// Local agents

export interface AgentDetection {
  installed: boolean;
  binary: string | null;
  config_path: string;
  config_exists: boolean;
}

export interface AgentConfigField {
  key: string;
  value: string | null;
  masked: boolean;
}

export interface AgentConfigView {
  path: string;
  exists: boolean;
  base_url: string | null;
  model: string | null;
  provider_configured: boolean;
  fields: AgentConfigField[];
  warnings: string[];
}

export interface AgentProfile {
  id: string;
  agent_id: string;
  name: string;
  base_url: string;
  virtual_model_id: string;
  virtual_key_id: string | null;
  config_path: string | null;
  created_at: string;
  updated_at: string;
}

export interface AgentBackup {
  id: string;
  agent_id: string;
  profile_id: string | null;
  path: string;
  original_sha256: string;
  applied_sha256: string;
  file_existed: boolean;
  created_at: string;
  restored_at: string | null;
}

export interface AgentView {
  id: string;
  name: string;
  vendor: string;
  detection: AgentDetection;
  config: AgentConfigView;
  configured: boolean;
  profile: AgentProfile | null;
  last_backup: AgentBackup | null;
  warnings: string[];
}

export interface AgentMode {
  sandbox: boolean;
  home: string;
  config_dir: string;
}

export interface AgentConfigChange {
  field: string;
  before: string | null;
  after: string | null;
}

export interface AgentPreview {
  agent_id: string;
  path: string;
  exists: boolean;
  current_sha256: string | null;
  changes: AgentConfigChange[];
  warnings: string[];
}

export interface AgentApplied {
  agent_id: string;
  path: string;
  backup_id: string;
  applied_at: string;
  changes: AgentConfigChange[];
  warnings: string[];
}

export interface AgentRestored {
  agent_id: string;
  path: string;
  backup_id: string;
  restored_at: string;
  removed: boolean;
}

export interface AgentProfileInput {
  name: string;
  base_url?: string;
  virtual_model_id: string;
  virtual_key_id?: string | null;
  config_path?: string | null;
}

export interface AgentRenderInput {
  profile_id?: string;
  base_url?: string;
  virtual_model_id?: string;
  virtual_key?: string;
  config_path?: string | null;
  expected_sha256?: string;
  force?: boolean;
}

// ---------------------------------------------------------------------------
// AI control plane

export interface AiCompileError {
  code: string;
  message: string;
}

export interface AiEntityCounts {
  providers: number;
  accounts: number;
  models: number;
  targets: number;
  policies: number;
  attachments: number;
}

export interface AiCompileReport {
  ok: boolean;
  errors: AiCompileError[];
  counts: AiEntityCounts;
}

export interface AiStatus {
  counts: AiEntityCounts;
  last_compile: AiCompileReport | null;
  revisions: number;
}

export interface AiProvider {
  id: string;
  name: string;
  protocol: string;
  base_url: string;
  auth_ref: string | null;
  capabilities: string[];
  config: unknown;
  enabled: boolean;
  created_at: number;
  updated_at: number;
}

export interface AiProviderAccount {
  id: string;
  provider_id: string;
  name: string;
  credential_kind: string;
  secret_ref: string | null;
  oauth_ref: string | null;
  oauth_status: string;
  oauth_expires_at: string | null;
  weight: number;
  priority: number;
  quota_limit: number | null;
  quota_unit: string;
  quota_period: string;
  quota_used: number;
  cooldown_until: string | null;
  enabled: boolean;
  config: unknown;
  created_at: number;
  updated_at: number;
}

export interface AiOAuthFlow {
  state: string;
  authorize_url: string;
  redirect_uri: string;
}

export interface AiOAuthStatus {
  account_id: string;
  provider_id: string;
  credential_kind: string;
  oauth_status: string;
  oauth_expires_at: string | null;
}

export type AiModelKind = "model" | "group";

export interface AiBalancer {
  strategy: string;
  retries: number;
  failover_criteria: string[];
}

export interface AiModel {
  id: string;
  name: string;
  kind: AiModelKind;
  capabilities: string[];
  formats: string[];
  route_paths: string[];
  balancer: AiBalancer;
  enabled: boolean;
  created_at: number;
  updated_at: number;
}

export interface AiModelTarget {
  id: string;
  model_id: string;
  target_model_id: string | null;
  provider_id: string | null;
  upstream_model: string | null;
  account_id: string | null;
  weight: number;
  priority: number;
  position: number;
  config: unknown;
  context_window: number | null;
  max_output_tokens: number | null;
  capabilities: unknown;
  pricing: unknown;
  semantic_description: string | null;
  enabled: boolean;
  created_at: number;
  updated_at: number;
}

export type AiPolicyType =
  | "allowlist"
  | "rate_limit"
  | "budget_guard"
  | "pii_redaction"
  | "prompt_guard"
  | "prompt_compress"
  | "semantic_cache"
  | "model_map"
  | "param_override"
  | "system_prompt"
  | "request_audit"
  | "think_tags";

export interface AiPolicy {
  id: string;
  name: string;
  policy_type: AiPolicyType;
  config: unknown;
  enabled: boolean;
  created_at: number;
  updated_at: number;
}

export type AiPolicyScope =
  "global" | "provider" | "model" | "target" | "consumer_group" | "consumer" | "virtual_key";

export interface AiPolicyAttachment {
  id: string;
  policy_id: string;
  scope: AiPolicyScope;
  scope_id: string | null;
  order: number;
  enabled: boolean;
  created_at: number;
  updated_at: number;
}

export interface AiRevision {
  id: string;
  created_at: number;
  actor: string;
  status: string;
  diff: unknown;
  applied_at: number | null;
  has_snapshot: boolean;
}

export interface AiApplyResult {
  ok: boolean;
  revision_id: string;
  report: AiCompileReport;
}

export interface AiRollbackResult {
  ok: boolean;
  revision_id: string;
  report: AiCompileReport;
}

// ---------------------------------------------------------------------------
// AI control-plane create/update payloads

export interface AiProviderInput {
  id?: string;
  name: string;
  protocol: string;
  base_url?: string;
  auth_ref?: string | null;
  capabilities?: string[];
  config?: unknown;
  enabled?: boolean;
}

export interface AiProviderAccountInput {
  id?: string;
  name: string;
  credential_kind?: string;
  secret_ref?: string | null;
  oauth_ref?: string | null;
  oauth_status?: string;
  oauth_expires_at?: string | null;
  weight?: number;
  priority?: number;
  quota_limit?: number | null;
  quota_unit?: string;
  quota_period?: string;
  quota_used?: number;
  cooldown_until?: string | null;
  config?: unknown;
  enabled?: boolean;
}

export interface AiModelInput {
  id?: string;
  name: string;
  kind?: AiModelKind;
  capabilities?: string[];
  formats?: string[];
  route_paths?: string[];
  balancer?: AiBalancer;
  enabled?: boolean;
}

export interface AiModelTargetInput {
  id?: string;
  target_model_id?: string | null;
  provider_id?: string | null;
  upstream_model?: string | null;
  account_id?: string | null;
  weight?: number;
  priority?: number;
  position?: number;
  config?: unknown;
  context_window?: number | null;
  max_output_tokens?: number | null;
  capabilities?: unknown;
  pricing?: unknown;
  semantic_description?: string | null;
  enabled?: boolean;
}

export interface AiPolicyInput {
  id?: string;
  name: string;
  policy_type: AiPolicyType;
  config?: unknown;
  enabled?: boolean;
}

export interface AiPolicyAttachmentInput {
  id?: string;
  scope: AiPolicyScope;
  scope_id?: string | null;
  order?: number;
  enabled?: boolean;
}
