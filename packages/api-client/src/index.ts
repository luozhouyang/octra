import type {
  ControlConfig,
  HealthResponse,
  PiiModelAction,
  PiiModelStatus,
  RouteSummary,
  ServiceSummary,
  SnapshotInfo,
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
    super(`control api request failed with status ${status}`);
    this.name = "ControlApiError";
    this.status = status;
    this.detail = detail;
  }
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

  services(signal?: AbortSignal): Promise<ServiceSummary[]> {
    return this.request<ServiceSummary[]>("/api/v1/services", { signal });
  }

  routes(signal?: AbortSignal): Promise<RouteSummary[]> {
    return this.request<RouteSummary[]>("/api/v1/routes", { signal });
  }

  reload(): Promise<SnapshotInfo> {
    return this.request<SnapshotInfo>("/api/v1/system/reload", { method: "POST" });
  }

  piiModels(signal?: AbortSignal): Promise<PiiModelStatus[]> {
    return this.request<PiiModelStatus[]>("/admin/llm/pii-models", { signal });
  }

  piiModelAction(
    id: string,
    action: PiiModelAction,
    signal?: AbortSignal,
  ): Promise<PiiModelStatus> {
    return this.request<PiiModelStatus>(
      `/admin/llm/pii-models/${encodeURIComponent(id)}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(action),
        signal,
      },
    );
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

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}
