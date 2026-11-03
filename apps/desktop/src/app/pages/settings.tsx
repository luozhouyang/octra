import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@octra/ui";
import type { PiiModelAction, PiiModelStatus } from "@octra/types";
import { useState, type FormEvent } from "react";

import { actOnPiiModel, fetchPiiModels, isTauri } from "../../lib/control-api";
import { useControlSettings } from "../../lib/store";

const inputClass =
  "mt-1 w-full rounded-md border border-border bg-background px-3 py-2 text-sm outline-none transition-colors focus:border-primary";

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024 * 1024) {
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  }
  if (bytes >= 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
  return `${Math.round(bytes / 1024)} KB`;
}

function PiiModelCard({
  model,
  busy,
  onAction,
}: {
  model: PiiModelStatus;
  busy: boolean;
  onAction: (action: PiiModelAction) => void;
}) {
  const [score, setScore] = useState(model.threshold);
  const download = model.state.state === "downloading" ? model.state : null;
  const downloading = download !== null;
  const percent =
    download && download.total > 0
      ? Math.min(100, (download.downloaded / download.total) * 100)
      : 0;

  return (
    <div className="mt-4 rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <p className="font-medium">{model.display_name}</p>
          <p className="mt-1 text-xs text-muted-foreground">
            {model.license} · {formatBytes(model.size)} · revision {model.revision.slice(0, 12)}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button
            type="button"
            disabled={busy}
            onClick={() => onAction({ do: model.enabled ? "off" : "on" })}
          >
            {model.enabled ? "Turn off" : "Turn on"}
          </Button>
          {!model.installed && !downloading && !model.enabled && (
            <span className="self-center text-xs text-muted-foreground">
              Turn on to download
            </span>
          )}
          {!model.installed && !downloading && model.enabled && (
            <Button type="button" disabled={busy} onClick={() => onAction({ do: "on" })}>
              Retry download
            </Button>
          )}
          {downloading && (
            <Button type="button" disabled={busy} onClick={() => onAction({ do: "cancel" })}>
              Cancel
            </Button>
          )}
          {model.installed && (
            <Button type="button" disabled={busy} onClick={() => onAction({ do: "remove" })}>
              Remove
            </Button>
          )}
          {model.engine_readiness === "failed" && (
            <Button type="button" disabled={busy} onClick={() => onAction({ do: "on" })}>
              Retry
            </Button>
          )}
        </div>
      </div>

      <div className="mt-3 space-y-2 text-sm">
        <p className="text-muted-foreground">
          Status:{" "}
          <span className="text-foreground">
            {model.enabled ? "enabled" : "off"} · {model.installed ? "installed" : model.state.state}
          </span>
        </p>
        {downloading && download && (
          <div>
            <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
              <div className="h-full bg-primary transition-all" style={{ width: `${percent}%` }} />
            </div>
            <p className="mt-1 text-xs text-muted-foreground">
              {formatBytes(download.downloaded)} / {formatBytes(download.total)}
            </p>
          </div>
        )}
        {model.last_error && <p className="text-xs text-danger">{model.last_error}</p>}
        {model.engine_readiness === "failed" && (
          <p className="text-xs text-danger">
            Engine failed to load{model.engine_error ? `: ${model.engine_error}` : ""}
          </p>
        )}
      </div>

      <div className="mt-4 flex flex-wrap items-end gap-3">
        <label className="text-sm">
          <span className="text-muted-foreground">Threshold ({model.floor}–0.99)</span>
          <input
            className={inputClass}
            type="number"
            min={model.floor}
            max={0.99}
            step={0.01}
            value={score}
            onChange={(event) => setScore(Number(event.target.value))}
          />
        </label>
        <Button
          type="button"
          disabled={busy}
          onClick={() => onAction({ do: "score", score })}
        >
          Apply threshold
        </Button>
      </div>
    </div>
  );
}

export function SettingsPage() {
  const queryClient = useQueryClient();
  const baseUrl = useControlSettings((state) => state.baseUrl);
  const token = useControlSettings((state) => state.token);
  const setBaseUrl = useControlSettings((state) => state.setBaseUrl);
  const setToken = useControlSettings((state) => state.setToken);

  const [draftBaseUrl, setDraftBaseUrl] = useState(baseUrl);
  const [draftToken, setDraftToken] = useState(token);
  const desktop = isTauri();

  const piiModels = useQuery({
    queryKey: ["pii-models"],
    queryFn: fetchPiiModels,
    refetchInterval: (query) =>
      query.state.data?.some((model) => model.state.state === "downloading") ? 1_000 : 5_000,
    retry: 1,
  });
  const piiAction = useMutation({
    mutationFn: ({ id, action }: { id: string; action: PiiModelAction }) =>
      actOnPiiModel(id, action),
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["pii-models"] }),
  });

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBaseUrl(draftBaseUrl.trim() || "http://127.0.0.1:8081");
    setToken(draftToken.trim());
    void queryClient.invalidateQueries({ queryKey: ["health"] });
  }

  return (
    <div className="space-y-8">
      <header>
        <h1 className="text-2xl font-semibold">Settings</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Local runtime and control plane connection.
        </p>
      </header>

      <section className="rounded-lg border border-border bg-card p-6">
        <h2 className="text-sm font-medium">Control plane</h2>

        {desktop ? (
          <p className="mt-3 text-sm text-muted-foreground">
            The desktop host reads credentials from{" "}
            <code className="rounded bg-background px-1.5 py-0.5">
              $OCTRA_HOME/control.token
            </code>{" "}
            and the daemon address from{" "}
            <code className="rounded bg-background px-1.5 py-0.5">$OCTRA_HOME/config.toml</code>.
          </p>
        ) : (
          <form className="mt-4 space-y-4" onSubmit={handleSubmit}>
            <label className="block text-sm">
              <span className="text-muted-foreground">Base URL</span>
              <input
                className={inputClass}
                value={draftBaseUrl}
                onChange={(event) => setDraftBaseUrl(event.target.value)}
                placeholder="http://127.0.0.1:8081"
              />
            </label>

            <label className="block text-sm">
              <span className="text-muted-foreground">Bearer token</span>
              <input
                className={inputClass}
                type="password"
                value={draftToken}
                onChange={(event) => setDraftToken(event.target.value)}
                placeholder="contents of $OCTRA_HOME/control.token"
              />
            </label>

            <Button type="submit">Save connection</Button>
          </form>
        )}
      </section>

      <section className="rounded-lg border border-border bg-card p-6">
        <h2 className="text-sm font-medium">Local PII model</h2>
        <p className="mt-3 text-sm text-muted-foreground">
          Optional on-device detection for names, addresses and secrets that rules miss. Models are
          downloaded only when you turn them on and are kept under{" "}
          <code className="rounded bg-background px-1.5 py-0.5">$OCTRA_HOME/cache/models</code>.
          Point the <code className="rounded bg-background px-1.5 py-0.5">llm-pii-redaction</code>{" "}
          plugin at the engine name{" "}
          <code className="rounded bg-background px-1.5 py-0.5">astrlink-guard</code> to use it.
        </p>

        {piiModels.isPending && (
          <p className="mt-4 text-sm text-muted-foreground">Loading models…</p>
        )}
        {piiModels.isError && (
          <p className="mt-4 text-sm text-danger">
            {piiModels.error instanceof Error ? piiModels.error.message : "cannot load models"}
          </p>
        )}
        {piiModels.data?.map((model) => (
          <PiiModelCard
            key={`${model.id}:${model.threshold}`}
            model={model}
            busy={piiAction.isPending && piiAction.variables?.id === model.id}
            onAction={(action) => piiAction.mutate({ id: model.id, action })}
          />
        ))}
        {piiAction.isError && (
          <p className="mt-4 text-sm text-danger">
            {piiAction.error instanceof Error ? piiAction.error.message : "model action failed"}
          </p>
        )}
      </section>

      <section className="rounded-lg border border-border bg-card p-6">
        <h2 className="text-sm font-medium">Local data</h2>
        <p className="mt-3 text-sm text-muted-foreground">
          SQLite, configuration, and the control token live under{" "}
          <code className="rounded bg-background px-1.5 py-0.5">$OCTRA_HOME</code>, which defaults
          to <code className="rounded bg-background px-1.5 py-0.5">~/.octra</code>.
        </p>
      </section>
    </div>
  );
}
