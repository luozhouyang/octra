import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Badge,
  Button,
  Card,
  CardDescription,
  CardFooter,
  CardHeader,
  CardSeparator,
  CardTitle,
  Segmented,
  cn,
} from "@octra/ui";
import type { ControlConfig, Overview, OverviewRange } from "@octra/types";
import {
  Activity,
  ChevronRight,
  Globe,
  Plug,
  RefreshCw,
  Route,
  Sparkles,
  Terminal,
} from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { RequestChart } from "../../components/request-chart";
import { StatStrip } from "../../components/stat-strip";
import {
  TrafficFabric,
  type FabricData,
  type FabricDestination,
  type FabricFlow,
} from "../../components/traffic-fabric";
import { controlClient, fetchHealth, reloadSnapshot } from "../../lib/control-api";
import { formatNumber, formatRelativeTime, formatUptime, hostFromBaseUrl } from "../../lib/format";
import { useControlSettings } from "../../lib/store";
import { toneSoft, toneText, type Tone } from "../../lib/tone";

const healthIcons: Record<string, typeof Route> = {
  "API Gateway": Route,
  "LLM Gateway": Sparkles,
  "MCP Gateway": Plug,
  "Control plane": Globe,
};

const gatewayColors: Record<string, string> = {
  api: "var(--octra-sky)",
  llm: "var(--octra-violet)",
  mcp: "var(--octra-amber)",
};

interface GatewayHealth {
  name: string;
  value: string;
  tone: "sky" | "violet" | "amber" | "primary";
}

export function Dashboard() {
  const queryClient = useQueryClient();
  const baseUrl = useControlSettings((state) => state.baseUrl);
  const [range, setRange] = useState<OverviewRange>("24h");

  const health = useQuery({
    queryKey: ["health"],
    queryFn: fetchHealth,
    refetchInterval: 5_000,
    retry: 1,
  });

  const overview = useQuery({
    queryKey: ["overview", range],
    queryFn: () => controlClient().overview(range),
    refetchInterval: 30_000,
    retry: 1,
  });

  const config = useQuery({
    queryKey: ["config"],
    queryFn: () => controlClient().config(),
    staleTime: 60_000,
    retry: 1,
  });

  const reload = useMutation({
    mutationFn: reloadSnapshot,
    onSettled: () => {
      void queryClient.invalidateQueries({ queryKey: ["health"] });
      void queryClient.invalidateQueries({ queryKey: ["overview"] });
    },
  });

  const data = health.data;
  const counts = data?.snapshot.counts;
  const online = data?.status === "ok";
  const updatedAt = health.dataUpdatedAt
    ? formatRelativeTime(new Date(health.dataUpdatedAt).toISOString())
    : "—";

  const traffic = overview.data;
  const totals = traffic?.totals;
  const success = totals && totals.requests > 0 ? 1 - totals.errors / totals.requests : 1;
  const avgLatency = weightedLatency(traffic);
  const chart = traffic ? buildChart(traffic) : emptyChart(range);
  const fabric = traffic ? buildFabric(traffic) : null;
  const insight = traffic ? topDestination(traffic) : null;
  const gateways = gatewayHealth(config.data, hostFromBaseUrl(baseUrl), online);

  const stats = [
    {
      label: "Services",
      value: String(counts?.services ?? 0),
      meta: "registered",
      tone: "primary" as Tone,
    },
    {
      label: "Routes",
      value: String(counts?.routes ?? 0),
      meta: "compiled into the snapshot",
      tone: "sky" as Tone,
    },
    {
      label: "Upstreams",
      value: String(counts?.upstreams ?? 0),
      meta: "target pools",
      tone: "amber" as Tone,
    },
    {
      label: "Targets",
      value: String(counts?.targets ?? 0),
      meta: "health-checked endpoints",
      tone: "coral" as Tone,
    },
  ];

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Runtime / local"
        title="Overview"
        description="Three gateways, one local runtime. Every request on this machine, from the caller that sent it to the upstream that answered."
        actions={
          <>
            <Button variant="secondary">
              <Terminal className="h-3.5 w-3.5" />
              CLI reference
            </Button>
            <Button onClick={() => reload.mutate()} disabled={reload.isPending}>
              <RefreshCw className={cn("h-3.5 w-3.5", reload.isPending && "animate-spin")} />
              Reload snapshot
            </Button>
          </>
        }
      />

      <section className="flex flex-wrap items-center gap-x-4 gap-y-2 border-y border-border py-3">
        <div className="flex items-center gap-2">
          <span
            className={cn(
              "h-1.5 w-1.5 rounded-full",
              health.isError ? "bg-danger" : online ? "bg-success" : "bg-warning",
            )}
          />
          <span className="text-xs font-medium">
            {health.isPending
              ? "Connecting"
              : health.isError
                ? "Daemon offline"
                : online
                  ? "All gateways operational"
                  : "Partial outage"}
          </span>
        </div>
        <span className="font-mono text-[10.5px] text-muted-foreground">
          control plane · {hostFromBaseUrl(baseUrl)} · v{data?.version ?? "—"} ·{" "}
          {data ? formatUptime(data.uptime_secs) : "—"} uptime ·{" "}
          {data?.storage === "ok" ? "SQLite WAL" : "storage error"} · snapshot {updatedAt}
        </span>
        <Badge variant={online ? "accent" : "warning"} className="ml-auto">
          {online ? "Healthy" : health.isError ? "Offline" : "Degraded"}
        </Badge>
      </section>

      <Card className="overflow-hidden">
        <CardHeader className="items-start gap-6">
          <div className="flex min-w-0 flex-col gap-1">
            <div className="flex items-center gap-2">
              <Activity className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
              <CardTitle>Traffic fabric</CardTitle>
            </div>
            <CardDescription>{chart.description}</CardDescription>
          </div>
          <div className="flex flex-wrap items-end gap-x-5 gap-y-3">
            <FabricStat label="Requests" value={formatNumber(totals?.requests ?? 0)} big />
            <FabricStat label="Success" value={`${(success * 100).toFixed(1)}%`} />
            <FabricStat
              label="Avg latency"
              value={avgLatency === null ? "—" : `${avgLatency} ms`}
            />
          </div>
        </CardHeader>
        <div className="overflow-x-auto border-y border-border bg-surface px-4 py-4 sm:px-5">
          {fabric && (totals?.requests ?? 0) > 0 ? (
            <TrafficFabric data={fabric} className="min-w-[860px]" />
          ) : (
            <div className="flex h-[360px] items-center justify-center text-xs text-muted-foreground">
              {overview.isPending
                ? "Loading traffic…"
                : overview.isError
                  ? "Cannot reach the control plane."
                  : "No traffic recorded in this window."}
            </div>
          )}
        </div>
        <CardFooter className="flex-wrap gap-3">
          <div className="flex items-center gap-2">
            <span className="h-1.5 w-3 rounded-full bg-primary" />
            <span className="text-[11px] text-muted-foreground">
              Ribbon width = requests · hover a flow to inspect
            </span>
          </div>
          <span className="ml-auto text-[11px] text-secondary">
            {insight ? `${insight.label} · ${insight.share}` : "No destination data"}
          </span>
        </CardFooter>
      </Card>

      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <Activity className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
                <CardTitle>Request volume</CardTitle>
              </div>
              <CardDescription>{chart.description}</CardDescription>
            </div>
            <Segmented
              value={range}
              onChange={setRange}
              options={[
                { value: "1h", label: "1H" },
                { value: "24h", label: "24H" },
                { value: "7d", label: "7D" },
              ]}
            />
          </CardHeader>
          <CardSeparator />
          <div className="flex flex-1 flex-col gap-3 p-4 sm:p-5">
            <div className="flex flex-wrap items-end justify-between gap-3">
              <div className="flex flex-wrap items-baseline gap-2.5">
                <span className="font-mono text-2xl font-medium">
                  {formatNumber(totals?.requests ?? 0)}
                </span>
                <span className="text-[11px] text-muted-foreground">requests</span>
                <Badge variant={totals && totals.errors > 0 ? "warning" : "accent"}>
                  {totals && totals.requests > 0
                    ? `${((totals.errors / totals.requests) * 100).toFixed(1)}% errors`
                    : "no errors"}
                </Badge>
              </div>
              <div className="flex items-center gap-3.5">
                <Legend color="bg-primary" label="Requests" />
                <Legend color="bg-coral" label="Errors" />
              </div>
            </div>
            <RequestChart values={chart.values} errors={chart.errors} />
            <div className="flex justify-between font-mono text-[10.5px] text-muted-foreground">
              {chart.labels.map((label, index) => (
                <span key={`${label}-${index}`}>{label}</span>
              ))}
            </div>
          </div>
          <CardFooter>
            <div className="flex items-center gap-2 text-[11px]">
              <span className="text-muted-foreground">avg latency</span>
              <span className="font-mono text-secondary">
                {avgLatency === null ? "—" : `${avgLatency} ms`}
              </span>
              <span className="text-muted-foreground">·</span>
              <span className="text-muted-foreground">error rate</span>
              <span className="font-mono text-secondary">{(1 - success).toFixed(1)}%</span>
            </div>
            <span className="font-mono text-[11px] text-muted-foreground">
              {formatNumber(totals?.requests ?? 0)} requests in the window
            </span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <ShieldIcon />
                <CardTitle>Gateway health</CardTitle>
              </div>
              <CardDescription>Data planes and control plane</CardDescription>
            </div>
            <Badge variant="accent">{online ? "Live" : "Offline"}</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="flex items-center gap-3.5 p-4 sm:p-5">
            <Donut value={success} />
            <div className="flex flex-col gap-0.5">
              <span className="font-mono text-[15px] font-medium">
                {(success * 100).toFixed(1)}%
              </span>
              <span className="text-[10.5px] text-muted-foreground">
                success in the selected window
              </span>
            </div>
            <Badge variant={success >= 0.99 ? "accent" : "warning"} className="ml-auto">
              {success >= 0.99 ? "Nominal" : "Attention"}
            </Badge>
          </div>
          <ul className="flex flex-col gap-1 px-4 pb-2">
            {gateways.map((row) => {
              const Icon = healthIcons[row.name] ?? Globe;
              return (
                <li key={row.name} className="flex h-9 items-center gap-2.5">
                  <span
                    className={cn(
                      "flex h-6 w-6 items-center justify-center rounded-md",
                      toneSoft[row.tone],
                    )}
                  >
                    <Icon className="h-3.5 w-3.5" strokeWidth={1.5} />
                  </span>
                  <span className="text-xs text-secondary">{row.name}</span>
                  <span className={cn("ml-auto font-mono text-[10.5px]", toneText[row.tone])}>
                    {row.value}
                  </span>
                </li>
              );
            })}
          </ul>
          <div className="flex-1" />
          <CardFooter>
            <span className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
              Last reload <span className="font-mono text-secondary">{updatedAt}</span>
            </span>
            <span className="flex items-center gap-1.5 text-[11px] text-secondary">
              View logs <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
            </span>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}

function buildFabric(overview: Overview): FabricData {
  const total = overview.totals.requests || 1;
  const gateways = [
    { id: "api", label: "API gateway", value: overview.totals.api },
    { id: "llm", label: "LLM gateway", value: overview.totals.llm },
    { id: "mcp", label: "MCP gateway", value: overview.totals.mcp },
  ].map((gateway) => ({
    ...gateway,
    display: formatNumber(gateway.value),
    share: `${((gateway.value / total) * 100).toFixed(1)}%`,
    color: gatewayColors[gateway.id] ?? "var(--octra-primary)",
  }));

  const destinations: FabricDestination[] = [];
  const flows: FabricFlow[] = [];
  const groups: Array<[string, Overview["destinations"]["api"]]> = [
    ["api", overview.destinations.api],
    ["llm", overview.destinations.llm],
    ["mcp", overview.destinations.mcp],
  ];

  let index = 0;
  for (const [gateway, rows] of groups) {
    for (const row of rows) {
      const id = `${gateway}-${index}`;
      destinations.push({
        id,
        label: row.label,
        value: row.count,
        display: formatNumber(row.count),
        share: `${((row.count / total) * 100).toFixed(1)}%`,
      });
      flows.push({ gateway, destination: id, value: row.count, lead: index === 0 });
      index += 1;
    }
  }

  return { gateways, destinations, flows };
}

function buildChart(overview: Overview): {
  values: number[];
  errors: number[];
  labels: string[];
  description: string;
  requests: number;
} {
  const requests = overview.series.api.map(
    (value, index) => value + (overview.series.llm[index] ?? 0) + (overview.series.mcp[index] ?? 0),
  );
  const requestMax = Math.max(1, ...requests);
  const errorMax = Math.max(1, ...overview.series.errors);

  return {
    values: requests.map((value) => value / requestMax),
    errors: overview.series.errors.map((value) => value / errorMax),
    labels: bucketLabels(overview),
    description: chartDescription(overview.range),
    requests: overview.totals.requests,
  };
}

function emptyChart(range: OverviewRange): {
  values: number[];
  errors: number[];
  labels: string[];
  description: string;
  requests: number;
} {
  return {
    values: [0, 0, 0, 0, 0, 0],
    errors: [0, 0, 0, 0, 0, 0],
    labels: ["", "", "", "", ""],
    description: chartDescription(range),
    requests: 0,
  };
}

function chartDescription(range: OverviewRange): string {
  const bucket = range === "1h" ? "5-minute" : range === "24h" ? "hourly" : "daily";
  return `All gateways · last ${range} · ${bucket} buckets`;
}

function bucketLabels(overview: Overview): string[] {
  const buckets = overview.buckets;
  if (buckets.length === 0) {
    return [];
  }
  const indexes = [
    0,
    Math.floor((buckets.length - 1) / 4),
    Math.floor((buckets.length - 1) / 2),
    Math.floor(((buckets.length - 1) * 3) / 4),
    buckets.length - 1,
  ];
  return [...new Set(indexes)].map((index) => formatBucket(buckets[index]!, overview.range));
}

function formatBucket(epochSeconds: number, range: OverviewRange): string {
  const date = new Date(epochSeconds * 1000);
  if (range === "7d") {
    return `${date.getMonth() + 1}/${date.getDate()}`;
  }
  return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
}

function weightedLatency(overview: Overview | undefined): number | null {
  if (!overview) {
    return null;
  }
  const weights: Array<[number | null, number]> = [
    [overview.latency.api_avg_ms, overview.totals.api],
    [overview.latency.llm_avg_ms, overview.totals.llm],
    [overview.latency.mcp_avg_ms, overview.totals.mcp],
  ];
  let sum = 0;
  let count = 0;
  for (const [latency, requests] of weights) {
    if (latency !== null && requests > 0) {
      sum += latency * requests;
      count += requests;
    }
  }
  return count > 0 ? Math.round(sum / count) : null;
}

function topDestination(overview: Overview): { label: string; share: string } | null {
  const rows = [
    ...overview.destinations.api,
    ...overview.destinations.llm,
    ...overview.destinations.mcp,
  ];
  const top = rows.sort((left, right) => right.count - left.count)[0];
  if (!top || overview.totals.requests === 0) {
    return null;
  }
  return {
    label: top.label,
    share: `${((top.count / overview.totals.requests) * 100).toFixed(1)}% of traffic`,
  };
}

function gatewayHealth(
  config: ControlConfig | undefined,
  controlHost: string,
  online: boolean,
): GatewayHealth[] {
  const dataHost = normalizeBind(config?.gateway.listen_bind, "127.0.0.1:8080");
  return [
    { name: "API Gateway", value: `${dataHost}/api`, tone: "sky" },
    { name: "LLM Gateway", value: `${dataHost}/llm`, tone: "violet" },
    { name: "MCP Gateway", value: `${dataHost}/mcp`, tone: "amber" },
    { name: "Control plane", value: online ? controlHost : "offline", tone: "primary" },
  ];
}

function normalizeBind(bind: string | undefined, fallback: string): string {
  const value = (bind ?? fallback).trim();
  return value.replace(/^0\.0\.0\.0:/, "127.0.0.1:").replace(/^\[::\]:/, "127.0.0.1:");
}

function FabricStat({ label, value, big }: { label: string; value: string; big?: boolean }) {
  return (
    <div className="flex flex-col gap-1">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className="flex h-9 items-end">
        <span
          className={cn("font-mono font-medium leading-none", big ? "text-[36px]" : "text-[24px]")}
        >
          {value}
        </span>
      </span>
    </div>
  );
}

function Legend({ color, label }: { color: string; label: string }) {
  return (
    <span className="flex items-center gap-1.5">
      <span className={cn("h-[7px] w-[7px] rounded-[2px]", color)} />
      <span className="text-[11px] text-muted-foreground">{label}</span>
    </span>
  );
}

function Donut({ value }: { value: number }) {
  const radius = 24;
  const circumference = 2 * Math.PI * radius;
  return (
    <svg viewBox="0 0 54 54" className="h-[54px] w-[54px] shrink-0" aria-hidden>
      <circle cx="27" cy="27" r={radius} fill="none" stroke="var(--octra-raised)" strokeWidth="5" />
      <circle
        cx="27"
        cy="27"
        r={radius}
        fill="none"
        stroke="var(--octra-primary)"
        strokeWidth="5"
        strokeLinecap="round"
        strokeDasharray={`${value * circumference} ${circumference}`}
        transform="rotate(-90 27 27)"
      />
    </svg>
  );
}

function ShieldIcon() {
  return (
    <svg
      viewBox="0 0 16 16"
      className="h-4 w-4 text-muted-foreground"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M8 1.8 13 3.6v4.1c0 3-2 5.2-5 6.5-3-1.3-5-3.5-5-6.5V3.6L8 1.8Z" />
      <path d="m5.9 8 1.5 1.5L10.6 6.3" />
    </svg>
  );
}
