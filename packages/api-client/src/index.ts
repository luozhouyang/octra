import type {
  AgentApplied,
  AgentBackup,
  AgentConfigView,
  AgentPreview,
  AgentProfile,
  AgentProfileInput,
  AgentRenderInput,
  AgentRestored,
  AgentView,
  ApiMetricsSnapshot,
  ApiPluginStats,
  ControlConfig,
  CreatedLlmVirtualKey,
  HealthResponse,
  ListResponse,
  LlmModel,
  LlmPlugin,
  LlmPluginMetrics,
  LlmProvider,
  LlmRoute,
  LlmRoutingGroup,
  LlmUsage,
  LlmVirtualKey,
  McpInvocation,
  McpServer,
  McpSession,
  McpTool,
  Overview,
  OverviewRange,
  PiiModelAction,
  PiiModelStatus,
  PluginInstanceView,
  RequestLog,
  RouteSummary,
  RouteView,
  Service,
  ServiceSummary,
  SnapshotInfo,
  TargetView,
  Upstream,
} from "@octra/types";

export interface ControlApiClientOptions {
  baseUrl: string;
  token?: string;
  fetchImpl?: typeof fetch;
}

export class ControlApiError extends Error {
  readonly status: number;
  readonly detail: unknown;

  constructor(status: number, detail: unknown) {
    super(errorMessage(status, detail));
    this.name = "ControlApiError";
    this.status = status;
    this.detail = detail;
  }
}

function errorMessage(status: number, detail: unknown): string {
  if (detail && typeof detail === "object") {
    const value = detail as Record<string, unknown>;
    const message = value.error ?? value.message;
    if (typeof message === "string" && message.length > 0) {
      return message;
    }
  }
  return `control api request failed with status ${status}`;
}

export class ControlApiClient {
  private readonly baseUrl: string;
  private readonly token?: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options: ControlApiClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.token = options.token;
    this.fetchImpl = options.fetchImpl ?? globalThis.fetch.bind(globalThis);
  }

  health(signal?: AbortSignal): Promise<HealthResponse> {
    return this.request<HealthResponse>("/api/v1/system/health", { signal });
  }

  config(signal?: AbortSignal): Promise<ControlConfig> {
    return this.request<ControlConfig>("/api/v1/system/config", { signal });
  }

  overview(range: OverviewRange = "24h", signal?: AbortSignal): Promise<Overview> {
    return this.request<Overview>(`/api/v1/system/overview${query({ range })}`, { signal });
  }

  services(signal?: AbortSignal): Promise<ServiceSummary[]> {
    return this.request<ServiceSummary[]>("/api/v1/services", { signal });
  }

  routes(signal?: AbortSignal): Promise<RouteSummary[]> {
    return this.request<RouteSummary[]>("/api/v1/routes", { signal });
  }

  reload(): Promise<SnapshotInfo> {
    return this.request<SnapshotInfo>("/api/v1/system/reload", { method: "POST" });
  }

  // ----- API gateway -----------------------------------------------------

  apiServices(signal?: AbortSignal): Promise<ListResponse<Service>> {
    return this.request<ListResponse<Service>>(`/admin/services${query({ size: 1000 })}`, {
      signal,
    });
  }

  apiRoutes(signal?: AbortSignal): Promise<ListResponse<RouteView>> {
    return this.request<ListResponse<RouteView>>(`/admin/routes${query({ size: 1000 })}`, {
      signal,
    });
  }

  upstreams(signal?: AbortSignal): Promise<ListResponse<Upstream>> {
    return this.request<ListResponse<Upstream>>(`/admin/upstreams${query({ size: 1000 })}`, {
      signal,
    });
  }

  targets(signal?: AbortSignal): Promise<ListResponse<TargetView>> {
    return this.request<ListResponse<TargetView>>(`/admin/targets${query({ size: 1000 })}`, {
      signal,
    });
  }

  apiMetrics(signal?: AbortSignal): Promise<ApiMetricsSnapshot> {
    return this.request<ApiMetricsSnapshot>("/admin/metrics", { signal });
  }

  pluginInstances(signal?: AbortSignal): Promise<ListResponse<PluginInstanceView>> {
    return this.request<ListResponse<PluginInstanceView>>(
      `/admin/plugins${query({ size: 1000 })}`,
      {
        signal,
      },
    );
  }

  apiPluginMetrics(
    signal?: AbortSignal,
  ): Promise<{ data: Array<{ name: string; metrics: ApiPluginStats }> }> {
    return this.request<{ data: Array<{ name: string; metrics: ApiPluginStats }> }>(
      "/admin/plugin-metrics",
      { signal },
    );
  }

  requestLogs(limit = 100, signal?: AbortSignal): Promise<RequestLog[]> {
    return this.request<RequestLog[]>(`/admin/logs${query({ limit })}`, { signal });
  }

  // ----- LLM gateway -----------------------------------------------------

  llmProviders(signal?: AbortSignal): Promise<LlmProvider[]> {
    return this.request<LlmProvider[]>("/admin/llm/providers", { signal });
  }

  llmModels(signal?: AbortSignal): Promise<LlmModel[]> {
    return this.request<LlmModel[]>("/admin/llm/models", { signal });
  }

  llmRoutingGroups(signal?: AbortSignal): Promise<LlmRoutingGroup[]> {
    return this.request<LlmRoutingGroup[]>("/admin/llm/routing-groups", { signal });
  }

  llmRoutes(signal?: AbortSignal): Promise<LlmRoute[]> {
    return this.request<LlmRoute[]>("/admin/llm/routes", { signal });
  }

  llmVirtualKeys(signal?: AbortSignal): Promise<LlmVirtualKey[]> {
    return this.request<LlmVirtualKey[]>("/admin/llm/virtual-keys", { signal });
  }

  createLlmVirtualKey(body: {
    name: string;
    consumer_id?: string | null;
    permissions?: unknown;
  }): Promise<CreatedLlmVirtualKey> {
    return this.request<CreatedLlmVirtualKey>("/admin/llm/virtual-keys", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  llmUsage(limit = 50, signal?: AbortSignal): Promise<LlmUsage> {
    return this.request<LlmUsage>(`/admin/llm/usage${query({ limit })}`, { signal });
  }

  llmPlugins(signal?: AbortSignal): Promise<LlmPlugin[]> {
    return this.request<LlmPlugin[]>("/admin/llm/plugins", { signal });
  }

  llmPluginMetrics(signal?: AbortSignal): Promise<{ plugins: LlmPluginMetrics[] }> {
    return this.request<{ plugins: LlmPluginMetrics[] }>("/admin/llm/plugins/metrics", { signal });
  }

  setLlmPluginEnabled(id: string, enabled: boolean): Promise<unknown> {
    return this.request(`/admin/llm/plugins/${encodeURIComponent(id)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ enabled }),
    });
  }

  // ----- MCP gateway -----------------------------------------------------

  mcpServers(signal?: AbortSignal): Promise<McpServer[]> {
    return this.request<McpServer[]>("/api/v1/mcp/servers", { signal });
  }

  mcpTools(serverId?: string, signal?: AbortSignal): Promise<McpTool[]> {
    return this.request<McpTool[]>(`/api/v1/mcp/tools${query({ server_id: serverId })}`, {
      signal,
    });
  }

  mcpInvocations(limit = 100, serverId?: string, signal?: AbortSignal): Promise<McpInvocation[]> {
    return this.request<McpInvocation[]>(
      `/api/v1/mcp/invocations${query({ limit, server_id: serverId })}`,
      { signal },
    );
  }

  mcpSessions(signal?: AbortSignal): Promise<McpSession[]> {
    return this.request<McpSession[]>("/api/v1/mcp/sessions", { signal });
  }

  // ----- Local agents ----------------------------------------------------

  agents(signal?: AbortSignal): Promise<AgentView[]> {
    return this.request<AgentView[]>("/api/v1/agents", { signal });
  }

  agent(id: string, signal?: AbortSignal): Promise<AgentView> {
    return this.request<AgentView>(`/api/v1/agents/${encodeURIComponent(id)}`, { signal });
  }

  agentConfig(id: string, signal?: AbortSignal): Promise<AgentConfigView & { agent_id: string }> {
    return this.request<AgentConfigView & { agent_id: string }>(
      `/api/v1/agents/${encodeURIComponent(id)}/config`,
      { signal },
    );
  }

  agentProfiles(id: string, signal?: AbortSignal): Promise<AgentProfile[]> {
    return this.request<AgentProfile[]>(`/api/v1/agents/${encodeURIComponent(id)}/profiles`, {
      signal,
    });
  }

  createAgentProfile(id: string, body: AgentProfileInput): Promise<AgentProfile> {
    return this.request<AgentProfile>(`/api/v1/agents/${encodeURIComponent(id)}/profiles`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  updateAgentProfile(
    id: string,
    profileId: string,
    body: AgentProfileInput,
  ): Promise<AgentProfile> {
    return this.request<AgentProfile>(
      `/api/v1/agents/${encodeURIComponent(id)}/profiles/${encodeURIComponent(profileId)}`,
      {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      },
    );
  }

  deleteAgentProfile(id: string, profileId: string): Promise<void> {
    return this.request<void>(
      `/api/v1/agents/${encodeURIComponent(id)}/profiles/${encodeURIComponent(profileId)}`,
      { method: "DELETE" },
    );
  }

  previewAgent(id: string, body: AgentRenderInput): Promise<AgentPreview> {
    return this.request<AgentPreview>(`/api/v1/agents/${encodeURIComponent(id)}/preview`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  applyAgent(id: string, body: AgentRenderInput): Promise<AgentApplied> {
    return this.request<AgentApplied>(`/api/v1/agents/${encodeURIComponent(id)}/apply`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  restoreAgent(
    id: string,
    body: { backup_id?: string; force?: boolean } = {},
  ): Promise<AgentRestored> {
    return this.request<AgentRestored>(`/api/v1/agents/${encodeURIComponent(id)}/restore`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  agentBackups(id: string, signal?: AbortSignal): Promise<AgentBackup[]> {
    return this.request<AgentBackup[]>(`/api/v1/agents/${encodeURIComponent(id)}/backups`, {
      signal,
    });
  }

  // ----- PII -------------------------------------------------------------

  piiModels(signal?: AbortSignal): Promise<PiiModelStatus[]> {
    return this.request<PiiModelStatus[]>("/admin/llm/pii-models", { signal });
  }

  piiModelAction(
    id: string,
    action: PiiModelAction,
    signal?: AbortSignal,
  ): Promise<PiiModelStatus> {
    return this.request<PiiModelStatus>(`/admin/llm/pii-models/${encodeURIComponent(id)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(action),
      signal,
    });
  }

  private async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const headers = new Headers(init.headers);
    headers.set("Accept", "application/json");
    if (this.token) {
      headers.set("Authorization", `Bearer ${this.token}`);
    }

    const response = await this.fetchImpl(`${this.baseUrl}${path}`, { ...init, headers });
    const text = await response.text();
    const body: unknown = text ? safeJson(text) : null;

    if (!response.ok) {
      throw new ControlApiError(response.status, body);
    }

    return body as T;
  }
}

function query(params: Record<string, string | number | undefined | null>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== "") {
      search.set(key, String(value));
    }
  }
  const text = search.toString();
  return text ? `?${text}` : "";
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}
