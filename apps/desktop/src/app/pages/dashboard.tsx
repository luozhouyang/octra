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
import { TrafficFabric } from "../../components/traffic-fabric";
import { fetchHealth, reloadSnapshot } from "../../lib/control-api";
import { formatRelativeTime, formatUptime, hostFromBaseUrl } from "../../lib/format";
import {
  gatewayHealth,
  requestSeries,
  trafficFabric,
  trafficSeriesSummary,
} from "../../lib/sample-data";
import { useControlSettings } from "../../lib/store";
import { toneSoft, toneText, type Tone } from "../../lib/tone";

const healthIcons: Record<string, typeof Route> = {
  "API Gateway": Route,
  "LLM Gateway": Sparkles,
  "MCP Gateway": Plug,
  "Control plane": Globe,
};

export function Dashboard() {
  const queryClient = useQueryClient();
  const baseUrl = useControlSettings((state) => state.baseUrl);
  const [range, setRange] = useState<"1h" | "24h" | "7d">("24h");

  const health = useQuery({
    queryKey: ["health"],
    queryFn: fetchHealth,
    refetchInterval: 5_000,
    retry: 1,
  });

  const reload = useMutation({
    mutationFn: reloadSnapshot,
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["health"] }),
  });

  const data = health.data;
  const counts = data?.snapshot.counts;
  const online = data?.status === "ok";
  const updatedAt = health.dataUpdatedAt
    ? formatRelativeTime(new Date(health.dataUpdatedAt).toISOString())
    : "—";

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
            <CardDescription>
              Every request, from gateway to upstream · last 24 hours
            </CardDescription>
          </div>
          <div className="flex flex-wrap items-end gap-x-5 gap-y-3">
            <FabricStat label="Requests" value={trafficSeriesSummary.requests} big />
            <FabricStat label="Success" value={trafficSeriesSummary.success} />
            <FabricStat label="p95" value={trafficSeriesSummary.p95} />
            <Segmented
              className="ml-auto"
              value={range}
              onChange={setRange}
              options={[
                { value: "1h", label: "1H" },
                { value: "24h", label: "24H" },
                { value: "7d", label: "7D" },
              ]}
            />
          </div>
        </CardHeader>
        <div className="overflow-x-auto border-y border-border bg-surface px-4 py-4 sm:px-5">
          <TrafficFabric data={trafficFabric} className="min-w-[860px]" />
        </div>
        <CardFooter className="flex-wrap gap-3">
          <div className="flex items-center gap-2">
            <span className="h-1.5 w-3 rounded-full bg-primary" />
            <span className="text-[11px] text-muted-foreground">
              Ribbon width = requests · hover a flow to inspect
            </span>
          </div>
          <span className="ml-auto text-[11px] text-secondary">{trafficSeriesSummary.insight}</span>
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
              <CardDescription>
                All gateways · last 24 hours · 5-second buckets aggregated
              </CardDescription>
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
                  {trafficSeriesSummary.requests}
                </span>
                <span className="text-[11px] text-muted-foreground">requests</span>
                <Badge variant="accent">
                  <span aria-hidden>↗</span>
                  {trafficSeriesSummary.delta}
                </Badge>
              </div>
              <div className="flex items-center gap-3.5">
                <Legend color="bg-primary" label="Requests" />
                <Legend color="bg-coral" label="Errors" />
              </div>
            </div>
            <RequestChart values={requestSeries.values} errors={requestSeries.errors} />
            <div className="flex justify-between font-mono text-[10.5px] text-muted-foreground">
              <span>00:00</span>
              <span>06:00</span>
              <span>12:00</span>
              <span>18:00</span>
              <span>now</span>
            </div>
          </div>
          <CardFooter>
            <div className="flex items-center gap-2 text-[11px]">
              <span className="text-muted-foreground">p95</span>
              <span className="font-mono text-secondary">38 ms</span>
              <span className="text-muted-foreground">·</span>
              <span className="text-muted-foreground">error rate</span>
              <span className="font-mono text-secondary">0.7%</span>
            </div>
            <span className="font-mono text-[11px] text-muted-foreground">
              sampled from {trafficSeriesSummary.requests} requests
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
            <Badge variant="accent">3 / 3</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="flex items-center gap-3.5 p-4 sm:p-5">
            <Donut value={0.993} />
            <div className="flex flex-col gap-0.5">
              <span className="font-mono text-[15px] font-medium">99.3%</span>
              <span className="text-[10.5px] text-muted-foreground">success over 24 hours</span>
            </div>
            <Badge variant="accent" className="ml-auto">
              Nominal
            </Badge>
          </div>
          <ul className="flex flex-col gap-1 px-4 pb-2">
            {gatewayHealth.map((row) => {
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
