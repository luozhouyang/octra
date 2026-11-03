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
  | { do: "on" | "off" | "download" | "cancel" | "remove" | "pick" }
  | { do: "score"; score: number };
