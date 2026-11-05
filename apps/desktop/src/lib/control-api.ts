import { ControlApiClient } from "@octra/api-client";
import type { HealthResponse, PiiModelAction, PiiModelStatus, SnapshotInfo } from "@octra/types";
import { invoke } from "@tauri-apps/api/core";

import { useControlSettings } from "./store";

interface DaemonResponse {
  status: number;
  body: unknown;
}

export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Adapts the desktop host command to the fetch interface used by the shared
 * control client. The webview never holds the control token.
 */
async function desktopFetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
  const url = new URL(typeof input === "string" ? input : input.toString());
  const body = init?.body ? JSON.parse(String(init.body)) : null;
  let response: DaemonResponse;
  try {
    response = await invoke<DaemonResponse>("daemon_request", {
      method: init?.method ?? "GET",
      path: `${url.pathname}${url.search}`,
      body,
    });
  } catch (error) {
    throw error instanceof Error ? error : new Error(String(error));
  }

  return new Response(JSON.stringify(response.body), {
    status: response.status,
    headers: { "content-type": "application/json" },
  });
}

/** Shared client for the local control plane, in the browser or the desktop host. */
export function controlClient(): ControlApiClient {
  const { baseUrl, token } = useControlSettings.getState();
  return new ControlApiClient({
    baseUrl,
    token: token || undefined,
    fetchImpl: isTauri() ? desktopFetch : undefined,
  });
}

export function fetchHealth(): Promise<HealthResponse> {
  return controlClient().health();
}

export function reloadSnapshot(): Promise<SnapshotInfo> {
  return controlClient().reload();
}

export function fetchPiiModels(): Promise<PiiModelStatus[]> {
  return controlClient().piiModels();
}

export function actOnPiiModel(id: string, action: PiiModelAction): Promise<PiiModelStatus> {
  return controlClient().piiModelAction(id, action);
}
