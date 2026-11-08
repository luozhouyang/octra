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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  cn,
} from "@octra/ui";
import { Info, Pencil, Plus, RefreshCw, Server, Trash2 } from "lucide-react";
import { useState } from "react";

import type { McpAcl } from "@octra/types";

import { PageHeader } from "../../components/page-header";
import { StatStrip } from "../../components/stat-strip";
import { controlClient } from "../../lib/control-api";
import { formatNumber, formatRelativeTime } from "../../lib/format";
import { toneBar, toneDot, type Tone } from "../../lib/tone";
import { McpTypedForm } from "./mcp-forms";

type TransportFilter = "all" | "stdio" | "http";
type McpEntityKind = "server" | "tool" | "key" | "acl";

interface McpEditorState {
  kind: McpEntityKind;
  id: string | null;
  title: string;
  initial: unknown;
}

interface Notice {
  text: string;
  error: boolean;
  secret?: boolean;
}

interface EntityChoice {
  id: string;
  label: string;
  entity: unknown;
}

const barTones: Tone[] = ["primary", "sky", "violet", "amber", "coral"];

const ENTITY_KINDS: ReadonlyArray<{
  value: McpEntityKind;
  label: string;
  newLabel: string;
}> = [
  { value: "server", label: "Server", newLabel: "New server" },
  { value: "tool", label: "Tool", newLabel: "New tool" },
  { value: "key", label: "API key", newLabel: "New key" },
  { value: "acl", label: "ACL", newLabel: "New ACL" },
];

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
  const queryClient = useQueryClient();
  const [transport, setTransport] = useState<TransportFilter>("all");
  const [editor, setEditor] = useState<McpEditorState>(() => newEditor("server"));
  const [formNonce, setFormNonce] = useState(0);
  const [notice, setNotice] = useState<Notice | null>(null);

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
  const keysQuery = useQuery({
    queryKey: ["mcp-keys"],
    queryFn: () => controlClient().mcpKeys(),
    retry: 1,
  });
  const aclsQuery = useQuery({
    queryKey: ["mcp-acls"],
    queryFn: () => controlClient().mcpAcls(),
    retry: 1,
  });

  const servers = serversQuery.data ?? [];
  const tools = toolsQuery.data ?? [];
  const invocations = invocationsQuery.data ?? [];
  const sessions = sessionsQuery.data ?? [];
  const keys = keysQuery.data ?? [];
  const acls = aclsQuery.data ?? [];

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

  function invalidateMcp() {
    for (const key of ["mcp-servers", "mcp-tools", "mcp-keys", "mcp-acls", "mcp-invocations"]) {
      void queryClient.invalidateQueries({ queryKey: [key] });
    }
  }

  function resetEditor(kind: McpEntityKind) {
    setEditor(newEditor(kind));
    setFormNonce((value) => value + 1);
  }

  const save = useMutation({
    mutationFn: ({ target, body }: { target: McpEditorState; body: Record<string, unknown> }) =>
      saveMcpEntity(target, body),
    onSuccess: (result, variables) => {
      if (variables.target.kind === "key") {
        const keyValue = createdKeyValue(result);
        setNotice({
          text: keyValue
            ? `API key created. Copy it now — it is shown only once: ${keyValue}`
            : "API key created.",
          error: false,
          secret: keyValue !== null,
        });
      } else {
        setNotice({
          text: `${kindLabel(variables.target.kind)} ${
            variables.target.id ? "updated" : "created"
          }.`,
          error: false,
        });
      }
      resetEditor(variables.target.kind);
      invalidateMcp();
    },
    onError: (error) => setNotice({ text: errorMessage(error), error: true }),
  });

  const remove = useMutation({
    mutationFn: ({ kind, id }: { kind: McpEntityKind; id: string }) => deleteMcpEntity(kind, id),
    onSuccess: (_result, variables) => {
      setNotice({ text: `${kindLabel(variables.kind)} deleted.`, error: false });
      if (editor.kind === variables.kind && editor.id === variables.id) {
        resetEditor(variables.kind);
      }
      invalidateMcp();
    },
    onError: (error) => setNotice({ text: errorMessage(error), error: true }),
  });

  const refresh = useMutation({
    mutationFn: (id: string) => controlClient().mcpRefreshServer(id),
    onSuccess: (_result, id) => {
      setNotice({ text: `Server ${id} refreshed.`, error: false });
      invalidateMcp();
    },
    onError: (error) => setNotice({ text: errorMessage(error), error: true }),
  });

  const busy = save.isPending || remove.isPending || refresh.isPending;
  const choices = entityChoices(editor.kind);
  const selectedChoice = choices.find((choice) => choice.id === editor.id) ?? null;

  function toolLabel(toolId: string): string {
    return tools.find((tool) => tool.id === toolId)?.qualified_name ?? toolId;
  }

  function aclScopeLabel(acl: McpAcl): string {
    if (acl.consumer_id) {
      return `consumer ${acl.consumer_id}`;
    }
    if (acl.consumer_group_id) {
      return `group ${acl.consumer_group_id}`;
    }
    return "default";
  }

  function entityChoices(kind: McpEntityKind): EntityChoice[] {
    switch (kind) {
      case "server":
        return servers.map((server) => ({
          id: server.id,
          label: `${server.name} · ${server.id}`,
          entity: server,
        }));
      case "tool":
        return tools.map((tool) => ({
          id: tool.id,
          label: `${tool.qualified_name} · ${tool.id}`,
          entity: tool,
        }));
      case "key":
        return keys.map((key) => ({
          id: key.id,
          label: `${key.name} · ${key.key_prefix} · ${key.id}`,
          entity: key,
        }));
      case "acl":
        return acls.map((acl) => ({
          id: acl.id,
          label: `${toolLabel(acl.tool_id)} · ${aclScopeLabel(acl)} · ${
            acl.allowed ? "allow" : "deny"
          } · ${acl.id}`,
          entity: acl,
        }));
    }
  }

  function openCreate(kind: McpEntityKind) {
    resetEditor(kind);
    setNotice(null);
    focusEditor();
  }

  function openEdit(kind: McpEntityKind, id: string) {
    const choice = entityChoices(kind).find((item) => item.id === id);
    if (!choice) {
      return;
    }
    setEditor({
      kind,
      id: choice.id,
      title: `Edit ${kindNoun(kind)} ${choice.id}`,
      initial: choice.entity,
    });
    setFormNonce((value) => value + 1);
    setNotice(null);
    focusEditor();
  }

  function changeKind(kind: McpEntityKind) {
    resetEditor(kind);
    setNotice(null);
  }

  function changeEntity(value: string) {
    if (value === "new") {
      openCreate(editor.kind);
      return;
    }
    openEdit(editor.kind, value);
  }

  function deleteWithConfirm(kind: McpEntityKind, id: string, label: string) {
    const confirmed = window.confirm(`Delete ${kindNoun(kind)} "${label}"? This cannot be undone.`);
    if (confirmed) {
      remove.mutate({ kind, id });
    }
  }

  function deleteCurrent() {
    if (!editor.id) {
      return;
    }
    deleteWithConfirm(editor.kind, editor.id, selectedChoice?.label ?? editor.id);
  }

  function refreshSelectedServer() {
    if (editor.kind === "server" && editor.id) {
      refresh.mutate(editor.id);
    }
  }

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
      />

      <StatStrip items={stats} />

      <Card id="mcp-entity-editor" className="flex flex-col">
        <CardHeader className="flex-wrap gap-3">
          <div className="flex flex-col gap-1">
            <CardTitle>Manage entity</CardTitle>
            <CardDescription>
              Create or edit MCP servers, tools, API keys and ACLs with typed forms.
            </CardDescription>
          </div>
          <div className="ml-auto flex flex-wrap items-center gap-1.5">
            {ENTITY_KINDS.map((item) => (
              <Button
                key={item.value}
                size="sm"
                variant="secondary"
                onClick={() => openCreate(item.value)}
                disabled={busy}
              >
                <Plus className="h-3.5 w-3.5" />
                {item.newLabel}
              </Button>
            ))}
          </div>
          <Badge variant={editor.id ? "accent" : "neutral"}>{editor.id ? "editing" : "new"}</Badge>
        </CardHeader>
        <CardSeparator />
        <div className="grid gap-4 p-4 lg:grid-cols-[minmax(190px,250px)_1fr]">
          <div className="flex flex-col gap-3">
            <label className="flex flex-col gap-1">
              <span className="text-[10.5px] text-muted-foreground">Kind</span>
              <select
                value={editor.kind}
                onChange={(event) => changeKind(readKind(event.target.value))}
                className="h-9 w-full rounded-lg border border-border bg-surface px-2 font-mono text-[11px] text-secondary outline-none transition-colors focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
              >
                {ENTITY_KINDS.map((item) => (
                  <option key={item.value} value={item.value}>
                    {item.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="flex flex-col gap-1">
              <span className="text-[10.5px] text-muted-foreground">Entity</span>
              <select
                value={editor.id ?? "new"}
                onChange={(event) => changeEntity(event.target.value)}
                className="h-9 w-full rounded-lg border border-border bg-surface px-2 font-mono text-[11px] text-secondary outline-none transition-colors focus:border-primary/40 focus:ring-2 focus:ring-primary/15"
              >
                <option value="new">New {kindNoun(editor.kind)}…</option>
                {editor.id !== null && !selectedChoice ? (
                  <option value={editor.id}>{editor.id} (missing)</option>
                ) : null}
                {choices.map((choice) => (
                  <option key={choice.id} value={choice.id}>
                    {choice.label}
                  </option>
                ))}
              </select>
            </label>
            <p className="text-[9.5px] leading-relaxed text-muted-foreground">
              {editorHint(editor.kind)}
            </p>
          </div>
          <div className="flex min-w-0 flex-col gap-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-[11px] font-medium text-foreground">{editor.title}</span>
              {editor.kind === "server" && editor.id ? (
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => refreshSelectedServer()}
                  disabled={busy}
                >
                  <RefreshCw className="h-3.5 w-3.5" />
                  {refresh.isPending ? "Refreshing…" : "Refresh"}
                </Button>
              ) : null}
            </div>
            <McpTypedForm
              key={`${editor.kind}-${editor.id ?? "new"}-${formNonce}`}
              kind={editor.kind}
              initial={editor.initial}
              servers={servers}
              tools={tools}
              pending={busy}
              onSave={(body) => save.mutate({ target: editor, body })}
              onCancel={() => openCreate(editor.kind)}
              onDelete={editor.id ? () => deleteCurrent() : undefined}
            />
            {notice ? (
              <span
                className={cn(
                  "text-[10.5px] break-all",
                  notice.error ? "text-danger" : "text-secondary",
                  notice.secret ? "font-mono" : "",
                )}
              >
                {notice.text}
              </span>
            ) : null}
          </div>
        </div>
        <CardFooter>
          <span className="text-[11px] text-muted-foreground">
            {servers.length} servers · {tools.length} tools · {keys.length} keys · {acls.length}{" "}
            ACLs
          </span>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            {editor.id ? `editing ${editor.id}` : "new entity"}
          </span>
        </CardFooter>
      </Card>

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
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {serversQuery.isLoading ? (
                <TableRow>
                  <TableCell colSpan={6} className="h-16 text-[11.5px] text-muted-foreground">
                    Loading servers…
                  </TableCell>
                </TableRow>
              ) : serversQuery.isError ? (
                <TableRow>
                  <TableCell colSpan={6} className="h-16 text-[11.5px] text-danger">
                    {serversError}
                  </TableCell>
                </TableRow>
              ) : visibleServers.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={6} className="h-16 text-[11.5px] text-muted-foreground">
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
                      <TableCell className="h-16">
                        <div className="flex items-center justify-end gap-1">
                          <Button
                            size="icon"
                            variant="ghost"
                            title={`Edit ${server.name}`}
                            onClick={() => openEdit("server", server.id)}
                            disabled={busy}
                          >
                            <Pencil className="h-3.5 w-3.5" />
                          </Button>
                          <Button
                            size="icon"
                            variant="ghost"
                            title={`Refresh ${server.name}`}
                            onClick={() => refresh.mutate(server.id)}
                            disabled={busy}
                          >
                            <RefreshCw className="h-3.5 w-3.5" />
                          </Button>
                          <Button
                            size="icon"
                            variant="ghost"
                            className="text-danger"
                            title={`Delete ${server.name}`}
                            onClick={() => deleteWithConfirm("server", server.id, server.name)}
                            disabled={busy}
                          >
                            <Trash2 className="h-3.5 w-3.5" />
                          </Button>
                        </div>
                      </TableCell>
                    </TableRow>
                  );
                })
              )}
            </TableBody>
          </Table>
          <div className="flex h-10 items-center justify-between bg-surface px-5">
            <span className="font-mono text-[10.5px] text-muted-foreground">Recent calls</span>
            <span className="font-mono text-[10.5px] text-muted-foreground">latest 200</span>
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
                      <div className="flex min-w-0 items-center gap-1">
                        <span className="truncate font-mono text-[11.5px]">{tool.name}</span>
                        <Button
                          size="icon"
                          variant="ghost"
                          className="h-6 w-6"
                          title={`Edit ${tool.name}`}
                          onClick={() => openEdit("tool", tool.id)}
                          disabled={busy}
                        >
                          <Pencil className="h-3 w-3" />
                        </Button>
                      </div>
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

function newEditor(kind: McpEntityKind): McpEditorState {
  return {
    kind,
    id: null,
    title: `New ${kindNoun(kind)}`,
    initial: {},
  };
}

function kindLabel(kind: McpEntityKind): string {
  switch (kind) {
    case "server":
      return "Server";
    case "tool":
      return "Tool";
    case "key":
      return "API key";
    case "acl":
      return "ACL";
  }
}

function kindNoun(kind: McpEntityKind): string {
  switch (kind) {
    case "server":
      return "server";
    case "tool":
      return "tool";
    case "key":
      return "API key";
    case "acl":
      return "ACL";
  }
}

function readKind(value: string): McpEntityKind {
  return ENTITY_KINDS.some((item) => item.value === value) ? (value as McpEntityKind) : "server";
}

function editorHint(kind: McpEntityKind): string {
  switch (kind) {
    case "server":
      return "stdio servers need command; http servers need url. Header secrets must use env:/keyring: references.";
    case "tool":
      return "Tools need server_id and name. Updates accept description, schema and enabled.";
    case "key":
      return "Keys return plaintext once. Pass consumer_id or consumer_name, not both; expires_at is YYYY-MM-DD.";
    case "acl":
      return "ACLs need tool_id and allowed, plus exactly one of consumer_id or consumer_group_id; omit both for the tool default. Updates only change allowed.";
  }
}

function saveMcpEntity(editor: McpEditorState, body: Record<string, unknown>): Promise<unknown> {
  const client = controlClient();
  switch (editor.kind) {
    case "server":
      return editor.id ? client.mcpUpdateServer(editor.id, body) : client.mcpCreateServer(body);
    case "tool":
      return editor.id ? client.mcpUpdateTool(editor.id, body) : client.mcpCreateTool(body);
    case "key":
      return client.mcpCreateKey(body);
    case "acl":
      return editor.id ? client.mcpUpdateAcl(editor.id, body) : client.mcpCreateAcl(body);
  }
}

function deleteMcpEntity(kind: McpEntityKind, id: string): Promise<void> {
  const client = controlClient();
  switch (kind) {
    case "server":
      return client.mcpDeleteServer(id);
    case "tool":
      return client.mcpDeleteTool(id);
    case "key":
      return client.mcpDeleteKey(id);
    case "acl":
      return client.mcpDeleteAcl(id);
  }
}

function createdKeyValue(result: unknown): string | null {
  if (result === null || typeof result !== "object") {
    return null;
  }
  const value = (result as { key_value?: unknown }).key_value;
  return typeof value === "string" ? value : null;
}

function focusEditor() {
  const node = document.getElementById("mcp-entity-editor");
  if (node && typeof node.scrollIntoView === "function") {
    node.scrollIntoView({ behavior: "smooth", block: "start" });
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "request failed";
}
