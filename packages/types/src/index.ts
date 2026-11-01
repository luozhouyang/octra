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
