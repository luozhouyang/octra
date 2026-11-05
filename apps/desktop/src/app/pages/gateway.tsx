import { useQuery } from "@tanstack/react-query";
import {
  Badge,
  Card,
  CardDescription,
  CardFooter,
  CardHeader,
  CardSeparator,
  CardTitle,
  Input,
  Segmented,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  cn,
} from "@octra/ui";
import { Search } from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip, type StatItem } from "../../components/stat-strip";
import { controlClient } from "../../lib/control-api";
import { formatNumber } from "../../lib/format";
import { toneDot, toneText } from "../../lib/tone";

type StatusFilter = "all" | "enabled" | "disabled";

export function GatewayPage() {
  const [filter, setFilter] = useState<StatusFilter>("all");
  const [query, setQuery] = useState("");

  const servicesQuery = useQuery({
    queryKey: ["api-services"],
    queryFn: () => controlClient().apiServices(),
    retry: 1,
  });
  const routesQuery = useQuery({
    queryKey: ["api-routes"],
    queryFn: () => controlClient().apiRoutes(),
    retry: 1,
  });
  const upstreamsQuery = useQuery({
    queryKey: ["api-upstreams"],
    queryFn: () => controlClient().upstreams(),
    retry: 1,
  });
  const targetsQuery = useQuery({
    queryKey: ["api-targets"],
    queryFn: () => controlClient().targets(),
    retry: 1,
  });
  const metricsQuery = useQuery({
    queryKey: ["api-metrics"],
    queryFn: () => controlClient().apiMetrics(),
    retry: 1,
  });

  const services = servicesQuery.data?.data ?? [];
  const routes = routesQuery.data?.data ?? [];
  const upstreams = upstreamsQuery.data?.data ?? [];
  const targets = targetsQuery.data?.data ?? [];
  const metrics = metricsQuery.data;

  const serviceNames = new Map(services.map((service) => [service.id, service.name] as const));

  const poolStats = new Map<string, { targets: number; weight: number }>();
  for (const target of targets) {
    const pool = poolStats.get(target.upstream_id) ?? { targets: 0, weight: 0 };
    pool.targets += 1;
    pool.weight += target.weight;
    poolStats.set(target.upstream_id, pool);
  }

  const enabledRoutes = routes.filter((route) => route.enabled).length;
  const enabledServices = services.filter((service) => service.enabled).length;

  const stats: StatItem[] = [
    {
      label: "Routes",
      value: String(routes.length),
      meta: `${enabledRoutes} enabled`,
      tone: "primary",
    },
    {
      label: "Upstreams",
      value: String(upstreams.length),
      meta: `${targets.length} targets`,
      tone: "amber",
    },
    {
      label: "Services",
      value: String(services.length),
      meta: `${enabledServices} enabled`,
      tone: "sky",
    },
    {
      label: "Requests",
      value: formatNumber(metrics?.requests_total ?? 0),
      meta: `avg ${(metrics?.latency_avg_ms ?? 0).toFixed(0)} ms`,
      tone: "coral",
    },
  ];

  const needle = query.trim().toLowerCase();
  const visibleRoutes = routes.filter((route) => {
    const serviceName = route.service_id ? serviceNames.get(route.service_id) : undefined;
    const matchesStatus =
      filter === "all" || (filter === "enabled" ? route.enabled : !route.enabled);
    const haystack = [...route.methods, ...route.paths, ...route.hosts, serviceName ?? ""]
      .join(" ")
      .toLowerCase();
    const matchesQuery = needle.length === 0 || haystack.includes(needle);
    return matchesStatus && matchesQuery;
  });

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Runtime / API"
        title="API Gateway"
        description="Routes, upstream pools and targets served by the HTTP data plane."
      />

      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader className="flex-wrap gap-3">
            <div className="flex flex-col gap-1">
              <CardTitle>Route table</CardTitle>
              <CardDescription>Matching order: exact, then prefix, then fallback</CardDescription>
            </div>
            <div className="ml-auto flex flex-wrap items-center gap-2">
              <div className="relative">
                <Search
                  className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground"
                  strokeWidth={1.5}
                />
                <Input
                  aria-label="Filter routes"
                  placeholder="Filter routes"
                  value={query}
                  onChange={(event) => setQuery(event.target.value)}
                  className="w-[190px] pl-7"
                />
              </div>
              <Segmented
                value={filter}
                onChange={setFilter}
                options={[
                  { value: "all", label: "All" },
                  { value: "enabled", label: "Enabled" },
                  { value: "disabled", label: "Disabled" },
                ]}
              />
            </div>
          </CardHeader>
          <CardSeparator />
          <Table>
            <TableHeader>
              <TableRow className="h-9 border-b border-border">
                <TableHead>Route</TableHead>
                <TableHead>Service</TableHead>
                <TableHead>Hosts</TableHead>
                <TableHead>Strip</TableHead>
                <TableHead>Status</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {routesQuery.isPending ? (
                <TableRow className="h-14">
                  <TableCell colSpan={5} className="text-center text-[11px] text-muted-foreground">
                    Loading routes…
                  </TableCell>
                </TableRow>
              ) : routesQuery.isError ? (
                <TableRow className="h-14">
                  <TableCell colSpan={5} className="text-center text-[11px] text-muted-foreground">
                    Cannot reach the control plane.{" "}
                    <span className="text-danger">{routesQuery.error.message}</span>
                  </TableCell>
                </TableRow>
              ) : visibleRoutes.length === 0 ? (
                <TableRow className="h-14">
                  <TableCell colSpan={5} className="text-center text-[11px] text-muted-foreground">
                    No routes registered.
                  </TableCell>
                </TableRow>
              ) : (
                visibleRoutes.map((route) => {
                  const serviceName = route.service_id
                    ? serviceNames.get(route.service_id)
                    : undefined;
                  const tone = route.enabled ? "primary" : "muted";
                  return (
                    <TableRow key={route.id} className="h-14">
                      <TableCell>
                        <div className="flex items-center gap-2">
                          <span className="rounded bg-raised px-1.5 py-0.5 font-mono text-[9.5px] text-secondary">
                            {route.methods[0] ?? "ANY"}
                          </span>
                          <span className="font-mono text-[11.5px]">{route.paths[0] ?? "*"}</span>
                        </div>
                      </TableCell>
                      <TableCell>
                        <span className="font-mono text-[11px] text-secondary">
                          {serviceName ?? "—"}
                        </span>
                      </TableCell>
                      <TableCell>
                        <span className="font-mono text-[11px] text-muted-foreground">
                          {route.hosts.length > 0 ? route.hosts.join(", ") : "*"}
                        </span>
                      </TableCell>
                      <TableCell>
                        <span className="font-mono text-[11px] text-muted-foreground">
                          {route.strip_path ? "yes" : "no"}
                        </span>
                      </TableCell>
                      <TableCell>
                        <div className="flex items-center gap-1.5">
                          <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[tone])} />
                          <span className={cn("text-[11px]", toneText[tone])}>
                            {route.enabled ? "Enabled" : "Disabled"}
                          </span>
                        </div>
                      </TableCell>
                    </TableRow>
                  );
                })
              )}
            </TableBody>
          </Table>
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              Showing {visibleRoutes.length} of {routes.length} routes
            </span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Upstream pools</CardTitle>
              <CardDescription>Algorithms and pooled target weights</CardDescription>
            </div>
            <Badge variant="accent">{upstreams.length} pools</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="flex flex-1 flex-col gap-2.5 p-4">
            {upstreamsQuery.isPending ? (
              <p className="text-[11px] text-muted-foreground">Loading upstreams…</p>
            ) : upstreamsQuery.isError ? (
              <p className="text-[11px] text-muted-foreground">
                Cannot reach the control plane.{" "}
                <span className="text-danger">{upstreamsQuery.error.message}</span>
              </p>
            ) : upstreams.length === 0 ? (
              <p className="text-[11px] text-muted-foreground">No upstreams registered.</p>
            ) : (
              upstreams.map((upstream) => {
                const pool = poolStats.get(upstream.id);
                return (
                  <div
                    key={upstream.id}
                    className="rounded-xl border border-border bg-surface p-3.5"
                  >
                    <div className="flex items-center justify-between gap-3">
                      <span className="font-mono text-xs">{upstream.name}</span>
                      <span className="text-[10.5px] text-muted-foreground">
                        {upstream.algorithm}
                      </span>
                    </div>
                    <div className="mt-2 flex items-center justify-between gap-3 text-[10.5px]">
                      <span className="text-muted-foreground">{pool?.targets ?? 0} targets</span>
                      <span className="font-mono text-secondary">
                        {formatNumber(pool?.weight ?? 0)} weight
                      </span>
                    </div>
                  </div>
                );
              })
            )}
          </div>
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">{targets.length} targets</span>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}
