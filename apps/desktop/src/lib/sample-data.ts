import type { RequestChartProps } from "../components/request-chart";
import type { FabricData } from "../components/traffic-fabric";
import type { Tone } from "./tone";

export type { Tone };

/*
 * Sample data stands in for endpoints the control API does not expose yet.
 * Counts, version, uptime and storage on the Overview come from the live
 * health endpoint; everything else here is a realistic preview of the shape
 * the API will return.
 */

export const trafficFabric: FabricData = {
  gateways: [
    {
      id: "api",
      label: "API gateway",
      value: 9247,
      display: "9,247",
      share: "48.8%",
      color: "var(--octra-sky)",
    },
    {
      id: "llm",
      label: "LLM gateway",
      value: 8412,
      display: "8,412",
      share: "44.4%",
      color: "var(--octra-violet)",
    },
    {
      id: "mcp",
      label: "MCP gateway",
      value: 1284,
      display: "1,284",
      share: "6.8%",
      color: "var(--octra-amber)",
    },
  ],
  destinations: [
    { id: "billing", label: "billing-api", value: 4218, display: "4,218", share: "22.3%" },
    { id: "identity", label: "identity", value: 2904, display: "2,904", share: "15.3%" },
    { id: "search", label: "search + other", value: 2125, display: "2,125", share: "11.2%" },
    { id: "openai", label: "OpenAI", value: 3868, display: "3,868", share: "20.4%" },
    { id: "anthropic", label: "Anthropic", value: 2355, display: "2,355", share: "12.4%" },
    { id: "ollama", label: "Ollama + Gemini", value: 2189, display: "2,189", share: "11.6%" },
    { id: "mcp", label: "MCP servers", value: 1284, display: "1,284", share: "6.8%" },
  ],
  flows: [
    { gateway: "api", destination: "billing", value: 4218, lead: true },
    { gateway: "api", destination: "identity", value: 2904 },
    { gateway: "api", destination: "search", value: 2125 },
    { gateway: "llm", destination: "openai", value: 3868 },
    { gateway: "llm", destination: "anthropic", value: 2355 },
    { gateway: "llm", destination: "ollama", value: 2189 },
    { gateway: "mcp", destination: "mcp", value: 1284 },
  ],
};

export const requestSeries: RequestChartProps = {
  values: [
    0.24, 0.21, 0.2, 0.26, 0.31, 0.29, 0.4, 0.44, 0.5, 0.47, 0.58, 0.78, 0.9, 0.86, 0.79, 0.84,
    0.88, 0.83, 0.9, 0.94, 0.92, 0.97, 0.99, 0.96,
  ],
  errors: [
    0.1, 0.12, 0.11, 0.14, 0.13, 0.15, 0.12, 0.1, 0.11, 0.13, 0.16, 0.14, 0.12, 0.1, 0.11, 0.09,
    0.12, 0.1, 0.08, 0.1, 0.11, 0.09, 0.1, 0.12,
  ],
};

export interface GatewayHealth {
  name: string;
  value: string;
  tone: "sky" | "violet" | "amber" | "primary";
}

export const gatewayHealth: GatewayHealth[] = [
  { name: "API Gateway", value: ":8080/api", tone: "sky" },
  { name: "LLM Gateway", value: ":8080/llm", tone: "violet" },
  { name: "MCP Gateway", value: ":8080/mcp", tone: "amber" },
  { name: "Control plane", value: "3 ms", tone: "primary" },
];

export interface RouteRow {
  method: string;
  path: string;
  upstream: string;
  targets: number;
  p95: string;
  status: "Healthy" | "Degraded";
  tone: Tone;
}

export const routeRows: RouteRow[] = [
  {
    method: "POST",
    path: "/v1/chat/completions",
    upstream: "llm-openai",
    targets: 3,
    p95: "412 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "GET",
    path: "/v1/models",
    upstream: "llm-openai",
    targets: 3,
    p95: "38 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "POST",
    path: "/v1/messages",
    upstream: "llm-anthropic",
    targets: 2,
    p95: "890 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "POST",
    path: "/v1/embeddings",
    upstream: "llm-openai",
    targets: 3,
    p95: "96 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "GET",
    path: "/billing/invoices",
    upstream: "billing-api",
    targets: 4,
    p95: "82 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "GET",
    path: "/identity/me",
    upstream: "identity-api",
    targets: 2,
    p95: "24 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "GET",
    path: "/search",
    upstream: "search-api",
    targets: 3,
    p95: "156 ms",
    status: "Degraded",
    tone: "amber",
  },
  {
    method: "POST",
    path: "/mcp/tools/call",
    upstream: "mcp-servers",
    targets: 2,
    p95: "61 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "GET",
    path: "/v1/responses",
    upstream: "llm-openai",
    targets: 2,
    p95: "340 ms",
    status: "Healthy",
    tone: "primary",
  },
  {
    method: "GET",
    path: "/healthz",
    upstream: "control",
    targets: 1,
    p95: "3 ms",
    status: "Healthy",
    tone: "primary",
  },
];

export interface UpstreamPool {
  name: string;
  meta: string;
  p95: string;
  status: "Healthy" | "Degraded";
  tone: Tone;
  fill: number;
}

export const upstreamPools: UpstreamPool[] = [
  {
    name: "billing-api",
    meta: "4 targets · 99.8% success",
    p95: "p95 82 ms",
    status: "Healthy",
    tone: "primary",
    fill: 0.8,
  },
  {
    name: "llm-openai",
    meta: "3 targets · 99.2% success",
    p95: "p95 412 ms",
    status: "Healthy",
    tone: "primary",
    fill: 0.8,
  },
  {
    name: "search-api",
    meta: "3 targets · 94.1% success",
    p95: "p95 156 ms",
    status: "Degraded",
    tone: "amber",
    fill: 0.55,
  },
];

export interface ModelRoute {
  model: string;
  provider: string;
  requests: string;
  tokens: string;
  spend: string;
  tone: Tone;
}

export const modelRoutes: ModelRoute[] = [
  {
    model: "gpt-4.1",
    provider: "openai",
    requests: "4,218",
    tokens: "6.1M",
    spend: "$18.24",
    tone: "primary",
  },
  {
    model: "gpt-4.1-mini",
    provider: "openai",
    requests: "2,904",
    tokens: "3.2M",
    spend: "$4.80",
    tone: "primary",
  },
  {
    model: "claude-sonnet-5.5",
    provider: "anthropic",
    requests: "2,355",
    tokens: "2.8M",
    spend: "$9.60",
    tone: "violet",
  },
  {
    model: "gemini-2.5-pro",
    provider: "google",
    requests: "1,642",
    tokens: "1.4M",
    spend: "$3.90",
    tone: "sky",
  },
  {
    model: "llama3.3:70b",
    provider: "ollama",
    requests: "547",
    tokens: "0.7M",
    spend: "$0.00",
    tone: "amber",
  },
  {
    model: "text-embedding-3-small",
    provider: "openai",
    requests: "890",
    tokens: "0.6M",
    spend: "$0.05",
    tone: "primary",
  },
  {
    model: "gpt-4o-mini",
    provider: "openai",
    requests: "412",
    tokens: "0.4M",
    spend: "$0.18",
    tone: "primary",
  },
  {
    model: "claude-haiku-5.5",
    provider: "anthropic",
    requests: "330",
    tokens: "0.3M",
    spend: "$0.06",
    tone: "violet",
  },
];

export interface ProviderShare {
  name: string;
  share: string;
  meta: string;
  tone: Tone;
  percent: number;
}

export const providerMix: ProviderShare[] = [
  { name: "OpenAI", share: "58%", meta: "4,873 requests · $23.27", tone: "primary", percent: 58 },
  { name: "Anthropic", share: "24%", meta: "2,019 requests · $9.66", tone: "violet", percent: 24 },
  { name: "Google", share: "12%", meta: "1,009 requests · $3.90", tone: "sky", percent: 12 },
  { name: "Ollama", share: "6%", meta: "511 requests · $0.00", tone: "amber", percent: 6 },
];

export interface McpServer {
  name: string;
  transport: "stdio" | "http";
  tools: number;
  calls: number;
  status: "Healthy" | "Degraded";
  tone: Tone;
}

export const mcpServers: McpServer[] = [
  {
    name: "filesystem",
    transport: "stdio",
    tools: 12,
    calls: 412,
    status: "Healthy",
    tone: "primary",
  },
  { name: "github", transport: "http", tools: 18, calls: 318, status: "Healthy", tone: "primary" },
  { name: "postgres", transport: "stdio", tools: 5, calls: 240, status: "Degraded", tone: "amber" },
  { name: "browser", transport: "stdio", tools: 3, calls: 154, status: "Healthy", tone: "primary" },
];

export interface McpTool {
  name: string;
  calls: number;
  percent: number;
  tone: Tone;
}

export const mcpTools: McpTool[] = [
  { name: "read_file", calls: 412, percent: 100, tone: "primary" },
  { name: "search_issues", calls: 318, percent: 77, tone: "primary" },
  { name: "query", calls: 240, percent: 58, tone: "sky" },
  { name: "open_url", calls: 154, percent: 37, tone: "sky" },
  { name: "list_tools", calls: 96, percent: 23, tone: "muted" },
  { name: "run_command", calls: 64, percent: 16, tone: "muted" },
];

export interface McpCall {
  tool: string;
  server: string;
  latency: string;
  when: string;
  tone: Tone;
}

export const mcpRecentCalls: McpCall[] = [
  { tool: "read_file", server: "filesystem", latency: "41 ms", when: "just now", tone: "primary" },
  { tool: "search_issues", server: "github", latency: "128 ms", when: "8s ago", tone: "primary" },
  { tool: "query", server: "postgres", latency: "310 ms", when: "14s ago", tone: "amber" },
];

export interface AgentSession {
  agent: string;
  model: string;
  steps: number;
  duration: string;
  status: "Completed" | "Running" | "Failed";
  tone: Tone;
}

export const agentSessions: AgentSession[] = [
  {
    agent: "release-notes",
    model: "gpt-4.1",
    steps: 24,
    duration: "3m 12s",
    status: "Completed",
    tone: "primary",
  },
  {
    agent: "repo-audit",
    model: "claude-sonnet-5.5",
    steps: 41,
    duration: "7m 48s",
    status: "Running",
    tone: "violet",
  },
  {
    agent: "ci-triage",
    model: "gpt-4.1-mini",
    steps: 12,
    duration: "1m 05s",
    status: "Completed",
    tone: "primary",
  },
  {
    agent: "doc-sync",
    model: "claude-haiku-5.5",
    steps: 8,
    duration: "42s",
    status: "Completed",
    tone: "primary",
  },
  {
    agent: "flaky-hunt",
    model: "gemini-2.5-pro",
    steps: 33,
    duration: "6m 20s",
    status: "Running",
    tone: "violet",
  },
  {
    agent: "cost-report",
    model: "llama3.3:70b",
    steps: 6,
    duration: "12s",
    status: "Failed",
    tone: "coral",
  },
];

export interface TraceStep {
  index: string;
  tool: string;
  server: string;
  latency: string;
  state: "ok" | "running";
}

export const agentTrace: TraceStep[] = [
  { index: "01", tool: "search_issues", server: "github", latency: "128 ms", state: "ok" },
  { index: "02", tool: "read_file", server: "filesystem", latency: "41 ms", state: "ok" },
  { index: "03", tool: "open_url", server: "browser", latency: "310 ms", state: "ok" },
  { index: "04", tool: "run_command", server: "filesystem", latency: "1.2 s", state: "ok" },
  { index: "05", tool: "write_file", server: "filesystem", latency: "now", state: "running" },
];

export interface PluginStage {
  index: string;
  name: string;
  description: string;
  stage: "request" | "response";
  latency: string;
  enabled: boolean;
}

export const pluginStages: PluginStage[] = [
  {
    index: "01",
    name: "intent_hint",
    description: "adds a routing hint to the prompt",
    stage: "request",
    latency: "0.4 ms",
    enabled: true,
  },
  {
    index: "02",
    name: "redaction",
    description: "masks PII before forwarding",
    stage: "request",
    latency: "2.1 ms",
    enabled: true,
  },
  {
    index: "03",
    name: "model_map",
    description: "maps aliases to upstream models",
    stage: "request",
    latency: "0.2 ms",
    enabled: true,
  },
  {
    index: "04",
    name: "param_override",
    description: "clamps temperature and max tokens",
    stage: "request",
    latency: "0.1 ms",
    enabled: true,
  },
  {
    index: "05",
    name: "system_prompt",
    description: "prepends the runtime prompt",
    stage: "request",
    latency: "0.3 ms",
    enabled: true,
  },
  {
    index: "06",
    name: "budget_guard",
    description: "rejects requests over budget",
    stage: "request",
    latency: "0.8 ms",
    enabled: true,
  },
  {
    index: "07",
    name: "rate_limit",
    description: "per-key token bucket",
    stage: "request",
    latency: "0.2 ms",
    enabled: false,
  },
  {
    index: "08",
    name: "think_tags",
    description: "strips reasoning tags from output",
    stage: "response",
    latency: "0.6 ms",
    enabled: true,
  },
  {
    index: "09",
    name: "request_audit",
    description: "records request metadata",
    stage: "response",
    latency: "0.5 ms",
    enabled: true,
  },
];

export interface PluginDecision {
  time: string;
  plugin: string;
  detail: string;
  outcome: "allow" | "block";
}

export const pluginDecisions: PluginDecision[] = [
  { time: "12:04:11", plugin: "redaction", detail: "masked 3 fields", outcome: "allow" },
  { time: "12:04:09", plugin: "budget_guard", detail: "within budget", outcome: "allow" },
  { time: "12:03:58", plugin: "rate_limit", detail: "429 returned", outcome: "block" },
  { time: "12:03:41", plugin: "request_audit", detail: "logged 8f21", outcome: "allow" },
  { time: "12:03:22", plugin: "param_override", detail: "temperature → 0.2", outcome: "allow" },
];

export type ChartRange = "1h" | "24h" | "7d";

export interface ChartView {
  series: RequestChartProps;
  requests: string;
  delta: string;
  description: string;
  labels: string[];
}

export function chartViewFor(range: ChartRange): ChartView {
  if (range === "1h") {
    return {
      series: {
        values: requestSeries.values.slice(-12),
        errors: (requestSeries.errors ?? []).slice(-12),
      },
      requests: "1,245",
      delta: "3.1% vs prev hour",
      description: "All gateways · last hour · 5-minute buckets",
      labels: ["-60m", "-45m", "-30m", "-15m", "now"],
    };
  }
  if (range === "7d") {
    return {
      series: {
        values: requestSeries.values.map((value) => Number((value * 7).toFixed(2))),
        errors: (requestSeries.errors ?? []).map((value) => Number((value * 7).toFixed(2))),
      },
      requests: "132,601",
      delta: "8.4% vs prev 7d",
      description: "All gateways · last 7 days · hourly buckets",
      labels: ["7d", "5d", "3d", "1d", "now"],
    };
  }
  return {
    series: requestSeries,
    requests: trafficSeriesSummary.requests,
    delta: trafficSeriesSummary.delta,
    description: "All gateways · last 24 hours · 5-second buckets aggregated",
    labels: ["00:00", "06:00", "12:00", "18:00", "now"],
  };
}

export const trafficSeriesSummary = {
  requests: "18,943",
  success: "99.3%",
  p95: "38 ms",
  delta: "8.4% vs prev 24h",
  insight: "Billing API · 22% of traffic",
};
