import { useQuery } from "@tanstack/react-query";
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
import { controlClient } from "../../lib/control-api";
import { formatNumber, formatRelativeTime } from "../../lib/format";
import { toneBar, toneDot, type Tone } from "../../lib/tone";

type TransportFilter = "all" | "stdio" | "http";

const barTones: Tone[] = ["primary", "sky", "violet", "amber", "coral"];

function statusTone(status: string): Tone {
  if (status === "ok") {
    return "primary";
  }
  if (status === "error") {
    return "danger";
  }
  return "amber";
}

export function McpPage() {
  const [transport, setTransport] = useState<TransportFilter>("all");

  const serversQuery = useQuery({
    queryKey: ["mcp-servers"],
    queryFn: () => controlClient().mcpServers(),
    retry: 1,
  });
  const toolsQuery = useQuery({
    queryKey: ["mcp-tools"],
    queryFn: () => controlClient().mcpTools(),
    retry: 1,
  });
  const invocationsQuery = useQuery({
    queryKey: ["mcp-invocations"],
    queryFn: () => controlClient().mcpInvocations(200),
    retry: 1,
  });
  const sessionsQuery = useQuery({
    queryKey: ["mcp-sessions"],
    queryFn: () => controlClient().mcpSessions(),
    retry: 1,
  });

  const servers = serversQuery.data ?? [];
  const tools = toolsQuery.data ?? [];
  const invocations = invocationsQuery.data ?? [];
  const sessions = sessionsQuery.data ?? [];

  const visibleServers =
    transport === "all" ? servers : servers.filter((server) => server.transport === transport);

  const enabledCount = servers.filter((server) => server.enabled).length;
  const errorCount = invocations.filter((call) => call.status !== "ok").length;
  const averageLatency =
    invocations.length > 0
      ? Math.round(
          invocations.reduce((sum, call) => sum + call.duration_ms, 0) / invocations.length,
        )
      : 0;

  const toolCountByServer = new Map<string, number>();
  for (const tool of tools) {
    toolCountByServer.set(tool.server_id, (toolCountByServer.get(tool.server_id) ?? 0) + 1);
  }

  const callsByServer = new Map<string, number>();
  for (const call of invocations) {
    callsByServer.set(call.server_id, (callsByServer.get(call.server_id) ?? 0) + 1);
  }

  const callCountByTool = new Map<string, number>();
  for (const call of invocations) {
    callCountByTool.set(call.tool_id, (callCountByTool.get(call.tool_id) ?? 0) + 1);
  }
  const topTools = [...callCountByTool.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, 6)
    .map(([toolId, count]) => ({
      id: toolId,
      name: tools.find((tool) => tool.id === toolId)?.qualified_name ?? toolId,
      count,
    }));
  const maxToolCalls = topTools[0]?.count ?? 0;

  const recentCalls = invocations.slice(0, 8);

  const serversError =
    serversQuery.error instanceof Error
      ? serversQuery.error.message
      : "Unable to load MCP servers.";

  const stats = [
    {
      label: "Servers",
      value: formatNumber(servers.length),
      meta: `${enabledCount} enabled`,
      tone: "primary" as Tone,
    },
    {
      label: "Tools",
      value: formatNumber(tools.length),
      meta: `across ${servers.length} servers`,
      tone: "violet" as Tone,
    },
    {
      label: "Recent calls",
      value: formatNumber(invocations.length),
      meta: `${errorCount} errors`,
      tone: "amber" as Tone,
    },
    {
      label: "Avg latency",
      value: `${formatNumber(averageLatency)} ms`,
      meta: `last ${invocations.length} calls`,
      tone: "sky" as Tone,
    },
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
              {serversQuery.isLoading ? (
                <TableRow>
                  <TableCell colSpan={5} className="h-16 text-[11.5px] text-muted-foreground">
                    Loading servers…
                  </TableCell>
                </TableRow>
              ) : serversQuery.isError ? (
                <TableRow>
                  <TableCell colSpan={5} className="h-16 text-[11.5px] text-danger">
                    {serversError}
                  </TableCell>
                </TableRow>
              ) : visibleServers.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={5} className="h-16 text-[11.5px] text-muted-foreground">
                    No MCP servers registered.
                  </TableCell>
                </TableRow>
              ) : (
                visibleServers.map((server) => {
                  const toolCount =
                    server.tool_count > 0
                      ? server.tool_count
                      : (toolCountByServer.get(server.id) ?? 0);
                  const callCount = callsByServer.get(server.id) ?? 0;
                  const serverTone: Tone = server.enabled ? "primary" : "muted";

                  return (
                    <TableRow key={server.id}>
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
                        <span className="font-mono text-[11.5px] text-secondary">{toolCount}</span>
                      </TableCell>
                      <TableCell className="h-16">
                        <span className="font-mono text-[11.5px] text-secondary">{callCount}</span>
                      </TableCell>
                      <TableCell className="h-16">
                        <span className="flex items-center gap-2">
                          <span
                            className={cn("h-[5px] w-[5px] rounded-full", toneDot[serverTone])}
                          />
                          <span className="text-[11px] text-secondary">
                            {server.enabled ? "Enabled" : "Disabled"}
                          </span>
                        </span>
                      </TableCell>
                    </TableRow>
                  );
                })
              )}
            </TableBody>
          </Table>
          <div className="flex h-10 items-center justify-between bg-surface px-5">
            <span className="font-mono text-[10.5px] text-muted-foreground">Recent calls</span>
            <span className="font-mono text-[10.5px] text-muted-foreground">live</span>
          </div>
          <div className="divide-y divide-border">
            {invocationsQuery.isPending ? (
              <div className="flex h-11 items-center px-5 text-[11px] text-muted-foreground">
                Loading calls…
              </div>
            ) : invocationsQuery.isError ? (
              <div className="flex h-11 items-center px-5 text-[11px] text-danger">
                {invocationsQuery.error instanceof Error
                  ? invocationsQuery.error.message
                  : "Unable to load invocations."}
              </div>
            ) : recentCalls.length === 0 ? (
              <div className="flex h-11 items-center px-5 text-[11px] text-muted-foreground">
                No tool calls recorded yet.
              </div>
            ) : (
              recentCalls.map((call) => {
                const toolName =
                  tools.find((tool) => tool.id === call.tool_id)?.name ?? call.tool_id;
                const serverName =
                  servers.find((server) => server.id === call.server_id)?.name ?? call.server_id;

                return (
                  <div
                    key={call.id}
                    title={call.error ?? undefined}
                    className="flex h-11 items-center gap-3 px-5"
                  >
                    <span
                      className={cn(
                        "h-[5px] w-[5px] rounded-full",
                        toneDot[statusTone(call.status)],
                      )}
                    />
                    <span className="font-mono text-[11px] text-secondary">{toolName}</span>
                    <span className="text-[11px] text-muted-foreground">{serverName}</span>
                    <span className="ml-auto font-mono text-[10.5px] text-muted-foreground">
                      {call.duration_ms} ms · {formatRelativeTime(call.started_at)}
                    </span>
                  </div>
                );
              })
            )}
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
              <span>{sessions.length} sessions</span>
              <span>{tools.length} tools</span>
            </div>
          </div>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Top tools</CardTitle>
              <CardDescription>Calls by tool · recent invocations</CardDescription>
            </div>
            <Badge variant="accent">{tools.length} tools</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="space-y-3.5 p-4">
            {topTools.length === 0 ? (
              <p className="text-[11px] text-muted-foreground">No tool calls recorded yet.</p>
            ) : (
              topTools.map((tool, index) => {
                const width = maxToolCalls > 0 ? Math.round((tool.count / maxToolCalls) * 100) : 0;

                return (
                  <div key={tool.id} className="flex flex-col gap-1.5">
                    <div className="flex items-center justify-between gap-3">
                      <span className="font-mono text-[11.5px]">{tool.name}</span>
                      <span className="font-mono text-[11px] text-secondary">{tool.count}</span>
                    </div>
                    <div className="h-[5px] w-full overflow-hidden rounded-full bg-raised">
                      <div
                        className={cn(
                          "h-full rounded-full",
                          toneBar[barTones[index % barTones.length] ?? "primary"],
                        )}
                        style={{ width: `${width}%` }}
                      />
                    </div>
                  </div>
                );
              })
            )}
          </div>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">Tool cache</span>
            <span className="font-mono text-[11px] text-muted-foreground">
              {tools.length} tools · {servers.length} servers
            </span>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}
