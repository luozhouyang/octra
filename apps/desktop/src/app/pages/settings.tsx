import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Badge,
  Button,
  Card,
  CardDescription,
  CardHeader,
  CardSeparator,
  CardTitle,
  Input,
  cn,
} from "@octra/ui";
import type { PiiModelAction, PiiModelStatus } from "@octra/types";
import { Database, HardDrive, ShieldCheck, SlidersHorizontal, type LucideIcon } from "lucide-react";
import { useState, type FormEvent, type ReactNode } from "react";

import { PageHeader } from "../../components/page-header";
import { actOnPiiModel, fetchPiiModels, isTauri } from "../../lib/control-api";
import { useControlSettings } from "../../lib/store";

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024 * 1024) {
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  }
  if (bytes >= 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
  return `${Math.round(bytes / 1024)} KB`;
}

const sections: Array<{ id: string; label: string; icon: LucideIcon }> = [
  { id: "connection", label: "Control plane", icon: SlidersHorizontal },
  { id: "pii", label: "Local PII model", icon: ShieldCheck },
  { id: "data", label: "Local data", icon: HardDrive },
];

function SettingRow({
  label,
  hint,
  children,
}: {
  label: string;
  hint: string;
  children: ReactNode;
}) {
  return (
    <div className="flex flex-col gap-3 border-b border-border px-5 py-4 last:border-b-0 sm:flex-row sm:items-center sm:justify-between">
      <div className="flex flex-col gap-0.5">
        <span className="text-xs text-foreground">{label}</span>
        <span className="text-[10.5px] text-muted-foreground">{hint}</span>
      </div>
      <div className="flex shrink-0 items-center gap-2">{children}</div>
    </div>
  );
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
    <div className="rounded-xl border border-border bg-surface p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex flex-col gap-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-xs font-medium text-foreground">{model.display_name}</span>
            <Badge variant={model.enabled ? "accent" : "neutral"}>
              {model.enabled ? "Enabled" : "Off"}
            </Badge>
            <Badge variant={model.installed ? "neutral" : downloading ? "violet" : "warning"}>
              {model.installed ? "Installed" : model.state.state}
            </Badge>
            {model.engine_readiness === "failed" ? (
              <Badge variant="danger">Engine failed</Badge>
            ) : null}
          </div>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            {model.license} · {formatBytes(model.size)} · revision {model.revision.slice(0, 12)}
          </span>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button
            size="sm"
            variant={model.enabled ? "secondary" : "default"}
            disabled={busy}
            onClick={() => onAction({ do: model.enabled ? "off" : "on" })}
          >
            {model.enabled ? "Turn off" : "Turn on"}
          </Button>
          {!model.installed && !downloading && !model.enabled ? (
            <span className="self-center text-[10.5px] text-muted-foreground">
              Turn on to download
            </span>
          ) : null}
          {!model.installed && !downloading && model.enabled ? (
            <Button
              size="sm"
              variant="secondary"
              disabled={busy}
              onClick={() => onAction({ do: "on" })}
            >
              Retry download
            </Button>
          ) : null}
          {downloading ? (
            <Button
              size="sm"
              variant="secondary"
              disabled={busy}
              onClick={() => onAction({ do: "cancel" })}
            >
              Cancel
            </Button>
          ) : null}
          {model.installed ? (
            <Button
              size="sm"
              variant="ghost"
              disabled={busy}
              onClick={() => onAction({ do: "remove" })}
            >
              Remove
            </Button>
          ) : null}
          {model.engine_readiness === "failed" ? (
            <Button
              size="sm"
              variant="secondary"
              disabled={busy}
              onClick={() => onAction({ do: "on" })}
            >
              Retry
            </Button>
          ) : null}
        </div>
      </div>

      {downloading && download ? (
        <div className="mt-3">
          <div className="h-1.5 w-full overflow-hidden rounded-full bg-raised">
            <div className="h-full bg-primary transition-all" style={{ width: `${percent}%` }} />
          </div>
          <p className="mt-1.5 font-mono text-[10.5px] text-muted-foreground">
            {formatBytes(download.downloaded)} / {formatBytes(download.total)}
          </p>
        </div>
      ) : null}

      {model.last_error ? (
        <p className="mt-3 text-[10.5px] text-danger">{model.last_error}</p>
      ) : null}
      {model.engine_readiness === "failed" ? (
        <p className="mt-2 text-[10.5px] text-danger">
          Engine failed to load{model.engine_error ? `: ${model.engine_error}` : ""}
        </p>
      ) : null}

      <div className="mt-4 flex flex-wrap items-end gap-3 border-t border-border pt-3">
        <label className="flex flex-col gap-1">
          <span className="text-[10.5px] text-muted-foreground">
            Threshold ({model.floor}–0.99)
          </span>
          <Input
            type="number"
            min={model.floor}
            max={0.99}
            step={0.01}
            value={score}
            onChange={(event) => setScore(Number(event.target.value))}
            className="w-[120px]"
          />
        </label>
        <Button
          size="sm"
          variant="secondary"
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
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="System / configuration"
        title="Settings"
        description="Runtime, storage and security settings for this daemon."
        actions={
          <>
            <Button
              variant="secondary"
              onClick={() => {
                setDraftBaseUrl(baseUrl);
                setDraftToken(token);
              }}
            >
              Reset
            </Button>
            <Button type="submit" form="connection-form">
              Save changes
            </Button>
          </>
        }
      />

      <div className="grid gap-4 lg:grid-cols-[240px_minmax(0,1fr)]">
        <Card className="h-fit p-4 lg:sticky lg:top-4">
          <span className="font-mono text-[10.5px] text-muted-foreground">Sections</span>
          <nav className="mt-2 flex flex-col gap-0.5">
            {sections.map((section, index) => (
              <a
                key={section.id}
                href={`#${section.id}`}
                className={cn(
                  "flex h-9 items-center gap-2.5 rounded-lg px-2.5 text-[12.5px] transition-colors",
                  index === 0
                    ? "bg-primary-soft text-foreground"
                    : "text-secondary hover:bg-raised hover:text-foreground",
                )}
              >
                <section.icon
                  className={cn(
                    "h-[15px] w-[15px]",
                    index === 0 ? "text-primary" : "text-muted-foreground",
                  )}
                  strokeWidth={1.5}
                />
                {section.label}
              </a>
            ))}
          </nav>
          <div className="my-3 h-px bg-border" />
          <div className="flex flex-col gap-1">
            <span className="font-mono text-[10.5px] text-muted-foreground">config.toml</span>
            <span className="text-[10.5px] text-muted-foreground">Edited 2h ago · local only</span>
          </div>
        </Card>

        <div className="flex min-w-0 flex-col gap-4">
          <Card id="connection">
            <CardHeader>
              <div className="flex flex-col gap-1">
                <CardTitle>Control plane</CardTitle>
                <CardDescription>Where this UI reaches the local daemon</CardDescription>
              </div>
              <Badge variant={desktop ? "neutral" : "accent"}>
                {desktop ? "Desktop" : "Browser"}
              </Badge>
            </CardHeader>
            <CardSeparator />
            {desktop ? (
              <div className="px-5 py-4 text-xs leading-5 text-secondary">
                The desktop host reads credentials from{" "}
                <code className="rounded bg-surface px-1.5 py-0.5 font-mono text-[11px]">
                  $OCTRA_HOME/control.token
                </code>{" "}
                and the daemon address from{" "}
                <code className="rounded bg-surface px-1.5 py-0.5 font-mono text-[11px]">
                  $OCTRA_HOME/config.toml
                </code>
                .
              </div>
            ) : (
              <form id="connection-form" onSubmit={handleSubmit}>
                <SettingRow label="Base URL" hint="Address of the local control API">
                  <Input
                    value={draftBaseUrl}
                    onChange={(event) => setDraftBaseUrl(event.target.value)}
                    placeholder="http://127.0.0.1:8081"
                    className="w-full sm:w-[260px]"
                  />
                </SettingRow>
                <SettingRow label="Bearer token" hint="Read from $OCTRA_HOME/control.token">
                  <Input
                    type="password"
                    value={draftToken}
                    onChange={(event) => setDraftToken(event.target.value)}
                    placeholder="Optional for loopback"
                    className="w-full sm:w-[260px]"
                  />
                </SettingRow>
                <div className="flex items-center justify-between px-5 py-3">
                  <span className="text-[10.5px] text-muted-foreground">
                    Applies to this browser only
                  </span>
                  <Button type="submit" size="sm">
                    Save connection
                  </Button>
                </div>
              </form>
            )}
          </Card>

          <Card id="pii">
            <CardHeader>
              <div className="flex flex-col gap-1">
                <CardTitle>Local PII model</CardTitle>
                <CardDescription>
                  Optional on-device detection for names, addresses and secrets that rules miss
                </CardDescription>
              </div>
              <Badge variant="neutral">
                <Database className="h-3 w-3" />
                {piiModels.data?.length ?? 0} {piiModels.data?.length === 1 ? "model" : "models"}
              </Badge>
            </CardHeader>
            <CardSeparator />
            <div className="flex flex-col gap-3 p-4">
              <p className="text-[11px] leading-5 text-muted-foreground">
                Models download only when you turn them on and are kept under{" "}
                <code className="rounded bg-surface px-1.5 py-0.5 font-mono text-[10.5px]">
                  $OCTRA_HOME/cache/models
                </code>
                . Point the <span className="font-mono">llm-pii-redaction</span> plugin at{" "}
                <span className="font-mono">astrlink-guard</span> to use it.
              </p>

              {piiModels.isPending ? (
                <p className="text-xs text-muted-foreground">Loading models…</p>
              ) : null}
              {piiModels.isError ? (
                <p className="text-xs text-danger">
                  {piiModels.error instanceof Error
                    ? piiModels.error.message
                    : "cannot load models"}
                </p>
              ) : null}
              {piiModels.data?.map((model) => (
                <PiiModelCard
                  key={`${model.id}:${model.threshold}`}
                  model={model}
                  busy={piiAction.isPending && piiAction.variables?.id === model.id}
                  onAction={(action) => piiAction.mutate({ id: model.id, action })}
                />
              ))}
              {piiAction.isError ? (
                <p className="text-xs text-danger">
                  {piiAction.error instanceof Error
                    ? piiAction.error.message
                    : "model action failed"}
                </p>
              ) : null}
            </div>
          </Card>

          <Card id="data">
            <CardHeader>
              <div className="flex flex-col gap-1">
                <CardTitle>Local data</CardTitle>
                <CardDescription>Everything stays on this machine</CardDescription>
              </div>
            </CardHeader>
            <CardSeparator />
            <div className="px-5 py-4 text-xs leading-5 text-secondary">
              SQLite, configuration and the control token live under{" "}
              <code className="rounded bg-surface px-1.5 py-0.5 font-mono text-[11px]">
                $OCTRA_HOME
              </code>
              , which defaults to{" "}
              <code className="rounded bg-surface px-1.5 py-0.5 font-mono text-[11px]">
                ~/.octra
              </code>
              .
            </div>
          </Card>
        </div>
      </div>
    </div>
  );
}
