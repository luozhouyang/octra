import {
  Badge,
  Button,
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
import { ArrowRight, FileCode, Plus, Search } from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip, type StatItem } from "../../components/stat-strip";
import { routeRows, upstreamPools } from "../../lib/sample-data";
import { toneBar, toneDot, toneText } from "../../lib/tone";

type StatusFilter = "all" | "healthy" | "degraded";

const stats: StatItem[] = [
  { label: "Routes", value: "34", meta: "4 shadows · 2 rewrites", tone: "primary" },
  { label: "Upstreams", value: "9", meta: "2 degraded", tone: "amber" },
  { label: "Targets", value: "18", meta: "3 unhealthy", tone: "coral" },
  { label: "Requests", value: "9,247", meta: "48.8% of traffic", tone: "sky" },
];

export function GatewayPage() {
  const [filter, setFilter] = useState<StatusFilter>("all");
  const [query, setQuery] = useState("");

  const needle = query.trim().toLowerCase();
  const visibleRoutes = routeRows.filter((row) => {
    const matchesStatus = filter === "all" || row.status.toLowerCase() === filter;
    const matchesQuery =
      needle.length === 0 ||
      row.method.toLowerCase().includes(needle) ||
      row.path.toLowerCase().includes(needle) ||
      row.upstream.toLowerCase().includes(needle);
    return matchesStatus && matchesQuery;
  });

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Runtime / API"
        title="API Gateway"
        description="Routes, upstream pools and targets served by the HTTP data plane."
        actions={
          <>
            <Button variant="secondary">
              <FileCode className="h-3.5 w-3.5" />
              View config
            </Button>
            <Button>
              <Plus className="h-3.5 w-3.5" />
              New route
            </Button>
          </>
        }
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
                  { value: "healthy", label: "Healthy" },
                  { value: "degraded", label: "Degraded" },
                ]}
              />
            </div>
          </CardHeader>
          <CardSeparator />
          <Table>
            <TableHeader>
              <TableRow className="h-9 border-b border-border">
                <TableHead>Route</TableHead>
                <TableHead>Upstream</TableHead>
                <TableHead>Targets</TableHead>
                <TableHead>p95</TableHead>
                <TableHead>Status</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {visibleRoutes.map((row) => (
                <TableRow key={`${row.method} ${row.path}`} className="h-14">
                  <TableCell>
                    <div className="flex items-center gap-2">
                      <span className="rounded bg-raised px-1.5 py-0.5 font-mono text-[9.5px] text-secondary">
                        {row.method}
                      </span>
                      <span className="font-mono text-[11.5px]">{row.path}</span>
                    </div>
                  </TableCell>
                  <TableCell>
                    <div className="flex items-center gap-1.5">
                      <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[row.tone])} />
                      <span className="font-mono text-[11px] text-secondary">{row.upstream}</span>
                    </div>
                  </TableCell>
                  <TableCell>
                    <span className="font-mono text-[11px] text-muted-foreground">
                      {row.targets}
                    </span>
                  </TableCell>
                  <TableCell>
                    <span className="font-mono text-[11px] text-secondary">{row.p95}</span>
                  </TableCell>
                  <TableCell>
                    <div className="flex items-center gap-1.5">
                      <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[row.tone])} />
                      <span className={cn("text-[11px]", toneText[row.tone])}>{row.status}</span>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              Showing {visibleRoutes.length} of 34 routes
            </span>
            <div className="flex items-center gap-2">
              <Button variant="secondary" size="sm">
                Previous
              </Button>
              <Button variant="secondary" size="sm" className="bg-raised">
                Next
              </Button>
            </div>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Upstream pools</CardTitle>
              <CardDescription>Health checked every 10 seconds</CardDescription>
            </div>
            <Badge variant="accent">9 pools</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="flex flex-1 flex-col gap-2.5 p-4">
            {upstreamPools.map((pool) => (
              <div key={pool.name} className="rounded-xl border border-border bg-surface p-3.5">
                <div className="flex items-center justify-between gap-3">
                  <span className="font-mono text-xs">{pool.name}</span>
                  <span className={cn("text-[10.5px]", toneText[pool.tone])}>{pool.status}</span>
                </div>
                <div className="mt-2 flex items-center justify-between gap-3 text-[10.5px]">
                  <span className="text-muted-foreground">{pool.meta}</span>
                  <span className="font-mono text-secondary">{pool.p95}</span>
                </div>
                <div className="mt-2.5 h-1 w-full overflow-hidden rounded-full bg-raised">
                  <div
                    className={cn("h-full rounded-full", toneBar[pool.tone])}
                    style={{ width: `${pool.fill * 100}%` }}
                  />
                </div>
              </div>
            ))}
          </div>
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">2 degraded targets</span>
            <Button variant="ghost" size="sm" className="-mr-2">
              Rebalance
              <ArrowRight className="h-3.5 w-3.5" />
            </Button>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}
