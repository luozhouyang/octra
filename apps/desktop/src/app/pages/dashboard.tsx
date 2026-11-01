import { useQuery } from "@tanstack/react-query";
import { Button } from "@octra/ui";

import { fetchHealth, isTauri } from "../../lib/control-api";

function MetricCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border border-border bg-card p-5">
      <p className="text-xs uppercase tracking-wider text-muted-foreground">{label}</p>
      <p className="mt-2 text-2xl font-semibold">{value}</p>
    </div>
  );
}

function formatUptime(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);

  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }
  if (minutes > 0) {
    return `${minutes}m ${seconds % 60}s`;
  }
  return `${seconds}s`;
}

export function Dashboard() {
  const health = useQuery({
    queryKey: ["health"],
    queryFn: fetchHealth,
    refetchInterval: 5_000,
    retry: 1,
  });

  const data = health.data;
  const counts = data?.snapshot.counts;

  const statusLabel = health.isPending
    ? "Connecting"
    : health.isError
      ? "Offline"
      : data?.status === "ok"
        ? "Online"
        : "Degraded";

  const statusDot = health.isError
    ? "bg-danger"
    : data?.status === "ok"
      ? "bg-success"
      : "bg-muted-foreground";

  const errorMessage = health.error instanceof Error ? health.error.message : "unknown error";

  return (
    <div className="space-y-8">
      <header className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold">Dashboard</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            Runtime status for this machine.
          </p>
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={() => void health.refetch()}
          disabled={health.isFetching}
        >
          {health.isFetching ? "Refreshing…" : "Refresh"}
        </Button>
      </header>

      <section className="rounded-lg border border-border bg-card p-6">
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="flex items-center gap-3">
            <span className={`h-2.5 w-2.5 rounded-full ${statusDot}`} />
            <div>
              <p className="text-sm font-medium">Gateway {statusLabel}</p>
              <p className="text-xs text-muted-foreground">
                {isTauri()
                  ? "Connected through the Tauri desktop host"
                  : "Connected directly over the loopback control API"}
              </p>
            </div>
          </div>
          <div className="text-right text-xs text-muted-foreground">
            <p>Version {data?.version ?? "—"}</p>
            <p>Uptime {data ? formatUptime(data.uptime_secs) : "—"}</p>
          </div>
        </div>

        {health.isError ? (
          <div className="mt-4 rounded-md border border-danger/40 bg-danger/10 p-4 text-sm">
            <p className="font-medium">Cannot reach the Octra daemon</p>
            <p className="mt-1 text-muted-foreground">{errorMessage}</p>
            <p className="mt-2 text-xs text-muted-foreground">
              Start it with{" "}
              <code className="rounded bg-background px-1.5 py-0.5">pnpm daemon</code> and try
              again.
            </p>
          </div>
        ) : null}
      </section>

      <section className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <MetricCard label="Services" value={String(counts?.services ?? 0)} />
        <MetricCard label="Routes" value={String(counts?.routes ?? 0)} />
        <MetricCard label="Upstreams" value={String(counts?.upstreams ?? 0)} />
        <MetricCard label="Targets" value={String(counts?.targets ?? 0)} />
      </section>

      <section className="rounded-lg border border-border bg-card p-6">
        <h2 className="text-sm font-medium">Runtime snapshot</h2>
        <dl className="mt-4 grid gap-4 text-sm sm:grid-cols-2">
          <div>
            <dt className="text-muted-foreground">Storage</dt>
            <dd className="mt-1 font-medium">{data?.storage ?? "unknown"}</dd>
          </div>
          <div>
            <dt className="text-muted-foreground">Generated</dt>
            <dd className="mt-1 font-medium">
              {data ? new Date(data.snapshot.generated_at).toLocaleString() : "—"}
            </dd>
          </div>
        </dl>
      </section>
    </div>
  );
}
