import { ControlApiClient } from "@octra/api-client";
import type { HealthResponse } from "@octra/types";
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
