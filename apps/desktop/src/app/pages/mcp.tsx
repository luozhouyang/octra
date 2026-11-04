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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  cn,
} from "@octra/ui";
import { Info, Plus, Server, Terminal } from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip } from "../../components/stat-strip";
import { mcpRecentCalls, mcpServers, mcpTools } from "../../lib/sample-data";
import { toneBar, toneDot, type Tone } from "../../lib/tone";

type TransportFilter = "all" | "stdio" | "http";

export function McpPage() {
  const [transport, setTransport] = useState<TransportFilter>("all");

  const servers =
    transport === "all"
      ? mcpServers
      : mcpServers.filter((server) => server.transport === transport);

  const stats = [
    { label: "Servers", value: "4", meta: "1 degraded", tone: "primary" as Tone },
    { label: "Tools", value: "38", meta: "across 4 servers", tone: "violet" as Tone },
    { label: "Calls", value: "1,284", meta: "6.8% of traffic", tone: "amber" as Tone },
    { label: "p95 latency", value: "61 ms", meta: "stdio transport", tone: "sky" as Tone },
  ];

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Runtime / MCP"
        title="MCP Gateway"
        description="Server registrations, tool discovery and call routing for local MCP clients."
        actions={
          <>
            <Button variant="secondary">
              <Terminal className="h-3.5 w-3.5" />
              Test call
            </Button>
            <Button>
              <Plus className="h-3.5 w-3.5" />
              Add server
            </Button>
          </>
        }
      />

      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <Server className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
                <CardTitle>MCP servers</CardTitle>
              </div>
              <CardDescription>Registered by the local runtime, not by upstreams</CardDescription>
            </div>
            <Segmented
              value={transport}
              onChange={setTransport}
              options={[
                { value: "all", label: "All" },
                { value: "stdio", label: "stdio" },
                { value: "http", label: "http" },
              ]}
            />
          </CardHeader>
          <CardSeparator />
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Server</TableHead>
                <TableHead>Transport</TableHead>
                <TableHead>Tools</TableHead>
                <TableHead>Calls</TableHead>
                <TableHead>Status</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {servers.map((server) => (
                <TableRow key={server.name}>
                  <TableCell className="h-16">
                    <div className="flex items-center gap-2.5">
                      <span className="flex h-6 w-6 items-center justify-center rounded-md bg-raised">
                        <Server className="h-3.5 w-3.5 text-secondary" strokeWidth={1.5} />
                      </span>
                      <span className="font-mono text-[11.5px]">{server.name}</span>
                    </div>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-[10.5px] text-secondary">
                      {server.transport}
                    </span>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-[11.5px] text-secondary">{server.tools}</span>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-[11.5px] text-secondary">{server.calls}</span>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="flex items-center gap-2">
                      <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[server.tone])} />
                      <span className="text-[11px] text-secondary">{server.status}</span>
                    </span>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          <div className="flex h-10 items-center justify-between bg-surface px-5">
            <span className="font-mono text-[10.5px] text-muted-foreground">Recent calls</span>
            <span className="font-mono text-[10.5px] text-muted-foreground">live</span>
          </div>
          <div className="divide-y divide-border">
            {mcpRecentCalls.map((call) => (
              <div key={`${call.tool}-${call.when}`} className="flex h-11 items-center gap-3 px-5">
                <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[call.tone])} />
                <span className="font-mono text-[11px] text-secondary">{call.tool}</span>
                <span className="text-[11px] text-muted-foreground">{call.server}</span>
                <span className="ml-auto font-mono text-[10.5px] text-muted-foreground">
                  {call.latency} · {call.when}
                </span>
              </div>
            ))}
          </div>
          <div className="flex-1" />
          <div className="space-y-2 p-4">
            <p className="flex items-start gap-2 text-[11px] text-muted-foreground">
              <Info className="h-3.5 w-3.5 shrink-0" strokeWidth={1.5} />
              <span>
                stdio servers restart with the daemon; http servers reconnect with backoff.
              </span>
            </p>
            <div className="flex items-center justify-between font-mono text-[10.5px] text-muted-foreground">
              <span>discovery: 60s</span>
              <span>last scan 12s ago</span>
            </div>
          </div>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Top tools</CardTitle>
              <CardDescription>Calls by tool · last 24 hours</CardDescription>
            </div>
            <Badge variant="accent">38 tools</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="space-y-3.5 p-4">
            {mcpTools.map((tool) => (
              <div key={tool.name} className="flex flex-col gap-1.5">
                <div className="flex items-center justify-between gap-3">
                  <span className="font-mono text-[11.5px]">{tool.name}</span>
                  <span className="font-mono text-[11px] text-secondary">{tool.calls}</span>
                </div>
                <div className="h-[5px] w-full overflow-hidden rounded-full bg-raised">
                  <div
                    className={cn("h-full rounded-full", toneBar[tool.tone])}
                    style={{ width: `${tool.percent}%` }}
                  />
                </div>
              </div>
            ))}
          </div>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">Tool cache</span>
            <span className="font-mono text-[11px] text-muted-foreground">
              38 tools · 4 servers
            </span>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}
