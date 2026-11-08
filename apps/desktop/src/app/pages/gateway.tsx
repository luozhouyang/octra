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
import { Pencil, Plus, Search, Trash2 } from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip, type StatItem } from "../../components/stat-strip";
import { controlClient } from "../../lib/control-api";
import { formatNumber } from "../../lib/format";
import { toneDot, toneText } from "../../lib/tone";
import { GatewayTypedForm, type GatewayEntityKind } from "./gateway-forms";

type StatusFilter = "all" | "enabled" | "disabled";

interface GatewayEditorState {
  kind: GatewayEntityKind;
  id: string | null;
  title: string;
  initial: unknown;
  revision: number;
}

interface Notice {
  text: string;
  error: boolean;
}

interface EntityChoice {
  id: string;
  label: string;
  entity: unknown;
}

const ENTITY_KINDS: ReadonlyArray<{ value: GatewayEntityKind; label: string }> = [
  { value: "service", label: "Service" },
  { value: "route", label: "Route" },
  { value: "upstream", label: "Upstream" },
  { value: "target", label: "Target" },
];

export function GatewayPage() {
  const queryClient = useQueryClient();
  const [filter, setFilter] = useState<StatusFilter>("all");
  const [query, setQuery] = useState("");
  const [editor, setEditor] = useState<GatewayEditorState>(() => newEditor("service"));
  const [notice, setNotice] = useState<Notice | null>(null);

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

  function invalidateGateway() {
    for (const key of [
      "api-services",
      "api-routes",
      "api-upstreams",
      "api-targets",
      "api-metrics",
    ]) {
      void queryClient.invalidateQueries({ queryKey: [key] });
    }
  }

  const save = useMutation({
    mutationFn: ({ target, body }: { target: GatewayEditorState; body: Record<string, unknown> }) =>
      saveGatewayEntity(target, body),
    onSuccess: (_result, variables) => {
      setNotice({
        text: `${kindLabel(variables.target.kind)} ${variables.target.id ? "updated" : "created"}.`,
        error: false,
      });
      setEditor((current) => newEditor(variables.target.kind, current.revision + 1));
      invalidateGateway();
    },
    onError: (error) => setNotice({ text: errorMessage(error), error: true }),
  });

  const remove = useMutation({
    mutationFn: ({ kind, id }: { kind: GatewayEntityKind; id: string }) =>
      deleteGatewayEntity(kind, id),
    onSuccess: (_result, variables) => {
      setNotice({ text: `${kindLabel(variables.kind)} deleted.`, error: false });
      if (editor.kind === variables.kind && editor.id === variables.id) {
        setEditor((current) => newEditor(variables.kind, current.revision + 1));
      }
      invalidateGateway();
    },
    onError: (error) => setNotice({ text: errorMessage(error), error: true }),
  });

  const busy = save.isPending || remove.isPending;
  const choices = entityChoices(editor.kind);
  const selectedChoice = choices.find((choice) => choice.id === editor.id) ?? null;

  function entityChoices(kind: GatewayEntityKind): EntityChoice[] {
    switch (kind) {
      case "service":
        return services.map((service) => ({
          id: service.id,
          label: `${service.name} · ${service.id}`,
          entity: service,
        }));
      case "route":
        return routes.map((route) => ({
          id: route.id,
          label: `${routeLabel(route)} · ${route.id}`,
          entity: route,
        }));
      case "upstream":
        return upstreams.map((upstream) => ({
          id: upstream.id,
          label: `${upstream.name} · ${upstream.id}`,
          entity: upstream,
        }));
      case "target":
        return targets.map((target) => ({
          id: target.id,
          label: `${target.target} → ${target.upstream_id} · ${target.id}`,
          entity: target,
        }));
    }
  }

  function openCreate(kind: GatewayEntityKind) {
    setEditor((current) => newEditor(kind, current.revision + 1));
    setNotice(null);
    focusEditor();
  }

  function openEdit(kind: GatewayEntityKind, id: string) {
    const choice = entityChoices(kind).find((item) => item.id === id);
    if (!choice) {
      return;
    }
    const initial = editableEntity(kind, choice.entity);
    setEditor((current) => ({
      kind,
      id: choice.id,
      title: `Edit ${kindLabel(kind).toLowerCase()} ${choice.id}`,
      initial,
      revision: current.revision + 1,
    }));
    setNotice(null);
    focusEditor();
  }

  function changeKind(kind: GatewayEntityKind) {
    setEditor((current) => newEditor(kind, current.revision + 1));
    setNotice(null);
  }

  function changeEntity(value: string) {
    if (value === "new") {
      openCreate(editor.kind);
      return;
    }
    openEdit(editor.kind, value);
  }

  function deleteWithConfirm(kind: GatewayEntityKind, id: string, label: string) {
    const confirmed = window.confirm(
      `Delete ${kindLabel(kind).toLowerCase()} "${label}"? This cannot be undone.`,
    );
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

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Runtime / API"
        title="API Gateway"
        description="Routes, upstream pools and targets served by the HTTP data plane."
      />

      <StatStrip items={stats} />

      <Card id="gateway-entity-editor" className="flex flex-col">
        <CardHeader>
          <div className="flex flex-col gap-1">
            <CardTitle>Manage entity</CardTitle>
            <CardDescription>
              Create or edit services, routes, upstreams and targets with typed forms. Writes reload
              the data plane.
            </CardDescription>
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
                <option value="new">New {kindLabel(editor.kind).toLowerCase()}…</option>
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
              {editor.kind === "target"
                ? "Editing prefills the API entity, including upstream_id. New targets must select an upstream in the form."
                : "Saving sends the typed entity to the Admin API and reloads the data plane. Headers and health checks stay under Advanced JSON."}
            </p>
          </div>
          <div className="flex min-w-0 flex-col gap-2">
            <span className="text-[10.5px] text-muted-foreground">{editor.title}</span>
            <GatewayTypedForm
              key={`${editor.kind}:${editor.id ?? "new"}:${editor.revision}`}
              kind={editor.kind}
              initial={editor.initial}
              services={services}
              upstreams={upstreams}
              pending={busy}
              onSave={(body) => save.mutate({ target: editor, body })}
              onCancel={() => openCreate(editor.kind)}
              onDelete={editor.id ? deleteCurrent : undefined}
            />
            {notice ? (
              <span
                className={cn("text-[10.5px]", notice.error ? "text-danger" : "text-secondary")}
              >
                {notice.text}
              </span>
            ) : null}
          </div>
        </div>
        <CardFooter>
          <span className="text-[11px] text-muted-foreground">
            {services.length} services · {routes.length} routes · {upstreams.length} upstreams ·{" "}
            {targets.length} targets
          </span>
          <span className="font-mono text-[10.5px] text-muted-foreground">
            {editor.id ? `editing ${editor.id}` : "new entity"}
          </span>
        </CardFooter>
      </Card>

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
              <Button
                size="sm"
                variant="secondary"
                disabled={busy}
                onClick={() => openCreate("route")}
              >
                <Plus className="h-3.5 w-3.5" />
                New route
              </Button>
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
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {routesQuery.isPending ? (
                <TableRow className="h-14">
                  <TableCell colSpan={6} className="text-center text-[11px] text-muted-foreground">
                    Loading routes…
                  </TableCell>
                </TableRow>
              ) : routesQuery.isError ? (
                <TableRow className="h-14">
                  <TableCell colSpan={6} className="text-center text-[11px] text-muted-foreground">
                    Cannot reach the control plane.{" "}
                    <span className="text-danger">{routesQuery.error.message}</span>
                  </TableCell>
                </TableRow>
              ) : visibleRoutes.length === 0 ? (
                <TableRow className="h-14">
                  <TableCell colSpan={6} className="text-center text-[11px] text-muted-foreground">
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
                      <TableCell className="text-right">
                        <div className="flex justify-end gap-1.5">
                          <Button
                            size="sm"
                            variant="secondary"
                            disabled={busy}
                            onClick={() => openEdit("route", route.id)}
                          >
                            <Pencil className="h-3.5 w-3.5" />
                            Edit
                          </Button>
                          <Button
                            size="sm"
                            variant="ghost"
                            className="text-danger hover:text-danger"
                            disabled={busy}
                            onClick={() => deleteWithConfirm("route", route.id, routeLabel(route))}
                          >
                            <Trash2 className="h-3.5 w-3.5" />
                            Delete
                          </Button>
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
          <CardHeader className="flex-wrap gap-3">
            <div className="flex flex-col gap-1">
              <CardTitle>Upstream pools</CardTitle>
              <CardDescription>Algorithms and pooled target weights</CardDescription>
            </div>
            <div className="ml-auto flex items-center gap-2">
              <Badge variant="accent">{upstreams.length} pools</Badge>
              <Button
                size="sm"
                variant="secondary"
                disabled={busy}
                onClick={() => openCreate("upstream")}
              >
                <Plus className="h-3.5 w-3.5" />
                New upstream
              </Button>
            </div>
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
                    <div className="mt-2 flex justify-end gap-1.5 border-t border-border/60 pt-2">
                      <Button
                        size="sm"
                        variant="ghost"
                        disabled={busy}
                        onClick={() => openEdit("upstream", upstream.id)}
                      >
                        <Pencil className="h-3.5 w-3.5" />
                        Edit
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="text-danger hover:text-danger"
                        disabled={busy}
                        onClick={() => deleteWithConfirm("upstream", upstream.id, upstream.name)}
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                        Delete
                      </Button>
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

function newEditor(kind: GatewayEntityKind, revision = 0): GatewayEditorState {
  return {
    kind,
    id: null,
    title: `New ${kindLabel(kind).toLowerCase()}`,
    initial: {},
    revision,
  };
}

function kindLabel(kind: GatewayEntityKind): string {
  switch (kind) {
    case "service":
      return "Service";
    case "route":
      return "Route";
    case "upstream":
      return "Upstream";
    case "target":
      return "Target";
  }
}

function readKind(value: string): GatewayEntityKind {
  return ENTITY_KINDS.some((item) => item.value === value)
    ? (value as GatewayEntityKind)
    : "service";
}

/**
 * The Admin API expands relations into `{ id }` objects on reads and prefers
 * them over the flat foreign keys on writes. Drop the relation object so edits
 * to `service_id` / `upstream_id` are honored.
 */
function editableEntity(kind: GatewayEntityKind, entity: unknown): unknown {
  if (entity === null || typeof entity !== "object" || Array.isArray(entity)) {
    return entity;
  }
  const copy = { ...(entity as Record<string, unknown>) };
  if (kind === "route") {
    delete copy.service;
  }
  if (kind === "target") {
    delete copy.upstream;
  }
  return copy;
}

function routeLabel(route: { methods: string[]; paths: string[] }): string {
  return `${route.methods[0] ?? "ANY"} ${route.paths[0] ?? "*"}`;
}

function saveGatewayEntity(
  editor: GatewayEditorState,
  body: Record<string, unknown>,
): Promise<unknown> {
  const client = controlClient();
  switch (editor.kind) {
    case "service":
      return editor.id ? client.apiUpdateService(editor.id, body) : client.apiCreateService(body);
    case "route":
      return editor.id ? client.apiUpdateRoute(editor.id, body) : client.apiCreateRoute(body);
    case "upstream":
      return editor.id ? client.apiUpdateUpstream(editor.id, body) : client.apiCreateUpstream(body);
    case "target":
      return editor.id ? client.apiUpdateTarget(editor.id, body) : client.apiCreateTarget(body);
  }
}

function deleteGatewayEntity(kind: GatewayEntityKind, id: string): Promise<void> {
  const client = controlClient();
  switch (kind) {
    case "service":
      return client.apiDeleteService(id);
    case "route":
      return client.apiDeleteRoute(id);
    case "upstream":
      return client.apiDeleteUpstream(id);
    case "target":
      return client.apiDeleteTarget(id);
  }
}

function focusEditor() {
  const node = document.getElementById("gateway-entity-editor");
  if (node && typeof node.scrollIntoView === "function") {
    node.scrollIntoView({ behavior: "smooth", block: "start" });
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "request failed";
}
