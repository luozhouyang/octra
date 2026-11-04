import { ControlApiClient } from "@octra/api-client";
import type { HealthResponse, PiiModelAction, PiiModelStatus, SnapshotInfo } from "@octra/types";
import { invoke } from "@tauri-apps/api/core";

import { useControlSettings } from "./store";

export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export async function fetchHealth(): Promise<HealthResponse> {
  if (isTauri()) {
    return invoke<HealthResponse>("daemon_health");
  }

  const { baseUrl, token } = useControlSettings.getState();
  return new ControlApiClient({ baseUrl, token: token || undefined }).health();
}

export async function reloadSnapshot(): Promise<SnapshotInfo> {
  if (isTauri()) {
    return invoke<SnapshotInfo>("daemon_reload");
  }

  const { baseUrl, token } = useControlSettings.getState();
  return new ControlApiClient({ baseUrl, token: token || undefined }).reload();
}

export async function fetchPiiModels(): Promise<PiiModelStatus[]> {
  if (isTauri()) {
    return invoke<PiiModelStatus[]>("daemon_pii_models");
  }

  const { baseUrl, token } = useControlSettings.getState();
  return new ControlApiClient({ baseUrl, token: token || undefined }).piiModels();
}

export async function actOnPiiModel(id: string, action: PiiModelAction): Promise<PiiModelStatus> {
  if (isTauri()) {
    return invoke<PiiModelStatus>("daemon_pii_action", { id, action });
  }

  const { baseUrl, token } = useControlSettings.getState();
  return new ControlApiClient({
    baseUrl,
    token: token || undefined,
  }).piiModelAction(id, action);
}
