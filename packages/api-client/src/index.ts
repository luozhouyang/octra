import type {
  AgentApplied,
  AgentBackup,
  AgentConfigView,
  AgentMode,
  AgentPreview,
  AgentProfile,
  AgentProfileInput,
  AgentRenderInput,
  AgentRestored,
  AgentView,
  AiApplyResult,
  AiCompileReport,
  AiModel,
  AiModelTarget,
  AiOAuthFlow,
  AiOAuthStatus,
  AiPolicy,
  AiPolicyAttachment,
  AiProvider,
  AiProviderAccount,
  AiRevision,
  AiRollbackResult,
  AiStatus,
  ApiMetricsSnapshot,
  ApiPluginStats,
  ControlConfig,
  CreatedLlmVirtualKey,
  CreatedMcpApiKey,
  HealthResponse,
  ListResponse,
  LlmPlugin,
  LlmPluginMetrics,
  LlmUsage,
  LlmVirtualKey,
  McpAcl,
  McpApiKey,
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

  apiCreateService(body: unknown): Promise<unknown> {
    return this.postJson<unknown>("/admin/services", body);
  }

  apiUpdateService(id: string, body: unknown): Promise<unknown> {
    return this.putJson<unknown>(`/admin/services/${encodeURIComponent(id)}`, body);
  }

  apiDeleteService(id: string): Promise<void> {
    return this.deleteEmpty(`/admin/services/${encodeURIComponent(id)}`);
  }

  apiCreateRoute(body: unknown): Promise<unknown> {
    return this.postJson<unknown>("/admin/routes", body);
  }

  apiUpdateRoute(id: string, body: unknown): Promise<unknown> {
    return this.putJson<unknown>(`/admin/routes/${encodeURIComponent(id)}`, body);
  }

  apiDeleteRoute(id: string): Promise<void> {
    return this.deleteEmpty(`/admin/routes/${encodeURIComponent(id)}`);
  }

  apiCreateUpstream(body: unknown): Promise<unknown> {
    return this.postJson<unknown>("/admin/upstreams", body);
  }

  apiUpdateUpstream(id: string, body: unknown): Promise<unknown> {
    return this.putJson<unknown>(`/admin/upstreams/${encodeURIComponent(id)}`, body);
  }

  apiDeleteUpstream(id: string): Promise<void> {
    return this.deleteEmpty(`/admin/upstreams/${encodeURIComponent(id)}`);
  }

  apiCreateTarget(body: unknown): Promise<unknown> {
    return this.postJson<unknown>("/admin/targets", body);
  }

  apiUpdateTarget(id: string, body: unknown): Promise<unknown> {
    return this.putJson<unknown>(`/admin/targets/${encodeURIComponent(id)}`, body);
  }

  apiDeleteTarget(id: string): Promise<void> {
    return this.deleteEmpty(`/admin/targets/${encodeURIComponent(id)}`);
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

  mcpCreateServer(body: unknown): Promise<McpServer> {
    return this.postJson<McpServer>("/api/v1/mcp/servers", body);
  }

  mcpUpdateServer(id: string, body: unknown): Promise<McpServer> {
    return this.putJson<McpServer>(`/api/v1/mcp/servers/${encodeURIComponent(id)}`, body);
  }

  mcpDeleteServer(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/mcp/servers/${encodeURIComponent(id)}`);
  }

  mcpRefreshServer(id: string): Promise<unknown> {
    return this.request<unknown>(`/api/v1/mcp/servers/${encodeURIComponent(id)}/refresh`, {
      method: "POST",
    });
  }

  mcpTools(serverId?: string, signal?: AbortSignal): Promise<McpTool[]> {
    return this.request<McpTool[]>(`/api/v1/mcp/tools${query({ server_id: serverId })}`, {
      signal,
    });
  }

  mcpCreateTool(body: unknown): Promise<McpTool> {
    return this.postJson<McpTool>("/api/v1/mcp/tools", body);
  }

  mcpUpdateTool(id: string, body: unknown): Promise<McpTool> {
    return this.putJson<McpTool>(`/api/v1/mcp/tools/${encodeURIComponent(id)}`, body);
  }

  mcpDeleteTool(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/mcp/tools/${encodeURIComponent(id)}`);
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

  mcpKeys(signal?: AbortSignal): Promise<McpApiKey[]> {
    return this.request<McpApiKey[]>("/api/v1/mcp/keys", { signal });
  }

  mcpCreateKey(body: unknown): Promise<CreatedMcpApiKey> {
    return this.postJson<CreatedMcpApiKey>("/api/v1/mcp/keys", body);
  }

  mcpDeleteKey(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/mcp/keys/${encodeURIComponent(id)}`);
  }

  mcpAcls(toolId?: string, signal?: AbortSignal): Promise<McpAcl[]> {
    return this.request<McpAcl[]>(`/api/v1/mcp/acls${query({ tool_id: toolId })}`, { signal });
  }

  mcpCreateAcl(body: unknown): Promise<McpAcl> {
    return this.postJson<McpAcl>("/api/v1/mcp/acls", body);
  }

  mcpUpdateAcl(id: string, body: unknown): Promise<McpAcl> {
    return this.putJson<McpAcl>(`/api/v1/mcp/acls/${encodeURIComponent(id)}`, body);
  }

  mcpDeleteAcl(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/mcp/acls/${encodeURIComponent(id)}`);
  }

  // ----- Local agents ----------------------------------------------------

  agents(signal?: AbortSignal): Promise<AgentView[]> {
    return this.request<AgentView[]>("/api/v1/agents", { signal });
  }

  agentMode(signal?: AbortSignal): Promise<AgentMode> {
    return this.request<AgentMode>("/api/v1/agents/mode", { signal });
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

  // ----- AI control plane ------------------------------------------------

  aiStatus(signal?: AbortSignal): Promise<AiStatus> {
    return this.request<AiStatus>("/api/v1/ai/status", { signal });
  }

  aiProviders(signal?: AbortSignal): Promise<AiProvider[]> {
    return this.request<AiProvider[]>("/api/v1/ai/providers", { signal });
  }

  aiCreateProvider(body: unknown): Promise<AiProvider> {
    return this.postJson<AiProvider>("/api/v1/ai/providers", body);
  }

  aiUpdateProvider(id: string, body: unknown): Promise<AiProvider> {
    return this.putJson<AiProvider>(`/api/v1/ai/providers/${encodeURIComponent(id)}`, body);
  }

  aiDeleteProvider(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/ai/providers/${encodeURIComponent(id)}`);
  }

  aiListAccounts(providerId: string): Promise<AiProviderAccount[]> {
    return this.request<AiProviderAccount[]>(
      `/api/v1/ai/providers/${encodeURIComponent(providerId)}/accounts`,
    );
  }

  aiCreateAccount(providerId: string, body: unknown): Promise<AiProviderAccount> {
    return this.postJson<AiProviderAccount>(
      `/api/v1/ai/providers/${encodeURIComponent(providerId)}/accounts`,
      body,
    );
  }

  aiUpdateAccount(id: string, body: unknown): Promise<AiProviderAccount> {
    return this.putJson<AiProviderAccount>(`/api/v1/ai/accounts/${encodeURIComponent(id)}`, body);
  }

  aiDeleteAccount(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/ai/accounts/${encodeURIComponent(id)}`);
  }

  aiAccountOAuthStart(accountId: string): Promise<AiOAuthFlow> {
    return this.postEmpty<AiOAuthFlow>(
      `/api/v1/ai/accounts/${encodeURIComponent(accountId)}/oauth/start`,
    );
  }

  aiAccountOAuthComplete(
    accountId: string,
    body: { code: string; state: string },
  ): Promise<AiOAuthStatus> {
    return this.postJson<AiOAuthStatus>(
      `/api/v1/ai/accounts/${encodeURIComponent(accountId)}/oauth/complete`,
      body,
    );
  }

  aiAccountOAuthRefresh(accountId: string): Promise<AiOAuthStatus> {
    return this.postEmpty<AiOAuthStatus>(
      `/api/v1/ai/accounts/${encodeURIComponent(accountId)}/oauth/refresh`,
    );
  }

  aiAccountOAuthStatus(accountId: string): Promise<AiOAuthStatus> {
    return this.request<AiOAuthStatus>(
      `/api/v1/ai/accounts/${encodeURIComponent(accountId)}/oauth/status`,
    );
  }

  aiAccountOAuthClear(accountId: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/ai/accounts/${encodeURIComponent(accountId)}/oauth`);
  }

  aiModels(signal?: AbortSignal): Promise<AiModel[]> {
    return this.request<AiModel[]>("/api/v1/ai/models", { signal });
  }

  aiCreateModel(body: unknown): Promise<AiModel> {
    return this.postJson<AiModel>("/api/v1/ai/models", body);
  }

  aiUpdateModel(id: string, body: unknown): Promise<AiModel> {
    return this.putJson<AiModel>(`/api/v1/ai/models/${encodeURIComponent(id)}`, body);
  }

  aiDeleteModel(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/ai/models/${encodeURIComponent(id)}`);
  }

  aiTargets(modelId: string, signal?: AbortSignal): Promise<AiModelTarget[]> {
    return this.request<AiModelTarget[]>(
      `/api/v1/ai/models/${encodeURIComponent(modelId)}/targets`,
      { signal },
    );
  }

  aiCreateTarget(modelId: string, body: unknown): Promise<AiModelTarget> {
    return this.postJson<AiModelTarget>(
      `/api/v1/ai/models/${encodeURIComponent(modelId)}/targets`,
      body,
    );
  }

  aiUpdateTarget(id: string, body: unknown): Promise<AiModelTarget> {
    return this.putJson<AiModelTarget>(`/api/v1/ai/targets/${encodeURIComponent(id)}`, body);
  }

  aiDeleteTarget(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/ai/targets/${encodeURIComponent(id)}`);
  }

  aiPolicies(signal?: AbortSignal): Promise<AiPolicy[]> {
    return this.request<AiPolicy[]>("/api/v1/ai/policies", { signal });
  }

  aiCreatePolicy(body: unknown): Promise<AiPolicy> {
    return this.postJson<AiPolicy>("/api/v1/ai/policies", body);
  }

  aiUpdatePolicy(id: string, body: unknown): Promise<AiPolicy> {
    return this.putJson<AiPolicy>(`/api/v1/ai/policies/${encodeURIComponent(id)}`, body);
  }

  aiDeletePolicy(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/ai/policies/${encodeURIComponent(id)}`);
  }

  aiAttachments(policyId: string, signal?: AbortSignal): Promise<AiPolicyAttachment[]> {
    return this.request<AiPolicyAttachment[]>(
      `/api/v1/ai/policies/${encodeURIComponent(policyId)}/attachments`,
      { signal },
    );
  }

  aiCreateAttachment(policyId: string, body: unknown): Promise<AiPolicyAttachment> {
    return this.postJson<AiPolicyAttachment>(
      `/api/v1/ai/policies/${encodeURIComponent(policyId)}/attachments`,
      body,
    );
  }

  aiUpdateAttachment(id: string, body: unknown): Promise<AiPolicyAttachment> {
    return this.putJson<AiPolicyAttachment>(
      `/api/v1/ai/attachments/${encodeURIComponent(id)}`,
      body,
    );
  }

  aiDeleteAttachment(id: string): Promise<void> {
    return this.deleteEmpty(`/api/v1/ai/attachments/${encodeURIComponent(id)}`);
  }

  aiRevisions(signal?: AbortSignal): Promise<AiRevision[]> {
    return this.request<AiRevision[]>("/api/v1/ai/revisions", { signal });
  }

  aiValidate(): Promise<AiCompileReport> {
    return this.postEmpty<AiCompileReport>("/api/v1/ai/validate");
  }

  aiApply(): Promise<AiApplyResult> {
    return this.postEmpty<AiApplyResult>("/api/v1/ai/apply");
  }

  aiRollback(id: string): Promise<AiRollbackResult> {
    return this.postEmpty<AiRollbackResult>(
      `/api/v1/ai/revisions/${encodeURIComponent(id)}/rollback`,
    );
  }

  private async postJson<T>(path: string, body: unknown): Promise<T> {
    return this.request<T>(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  private async putJson<T>(path: string, body: unknown): Promise<T> {
    return this.request<T>(path, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  private async deleteEmpty(path: string): Promise<void> {
    await this.request<void>(path, { method: "DELETE" });
  }

  private async postEmpty<T>(path: string): Promise<T> {
    return this.postJson<T>(path, {});
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
