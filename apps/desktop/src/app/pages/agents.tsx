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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  cn,
} from "@octra/ui";
import type { AgentPreview, AgentView } from "@octra/types";
import {
  ArrowRight,
  Bot,
  CheckCircle2,
  KeyRound,
  RefreshCw,
  RotateCcw,
  Save,
  ShieldCheck,
  Sparkles,
} from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip, type StatItem } from "../../components/stat-strip";
import { controlClient } from "../../lib/control-api";
import { formatRelativeTime } from "../../lib/format";
import { toneDot, toneText } from "../../lib/tone";

export function AgentsPage() {
  const queryClient = useQueryClient();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [profileName, setProfileName] = useState("default");
  const [baseUrl, setBaseUrl] = useState("http://127.0.0.1:8080");
  const [model, setModel] = useState("");
  const [keyId, setKeyId] = useState("");
  const [plaintextKey, setPlaintextKey] = useState("");
  const [preview, setPreview] = useState<AgentPreview | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const agents = useQuery({
    queryKey: ["agents"],
    queryFn: () => controlClient().agents(),
    refetchInterval: 10_000,
    retry: 1,
  });
  const routes = useQuery({
    queryKey: ["llm-routes"],
    queryFn: () => controlClient().llmRoutes(),
    retry: 1,
  });
  const keys = useQuery({
    queryKey: ["llm-virtual-keys"],
    queryFn: () => controlClient().llmVirtualKeys(),
    retry: 1,
  });
  const mode = useQuery({
    queryKey: ["agent-mode"],
    queryFn: () => controlClient().agentMode(),
    retry: 1,
  });
  const selected = useMemo(
    () => agents.data?.find((agent) => agent.id === selectedId) ?? agents.data?.[0] ?? null,
    [agents.data, selectedId],
  );
  const backups = useQuery({
    queryKey: ["agent-backups", selected?.id],
    queryFn: () => controlClient().agentBackups(selected!.id),
    enabled: selected !== null,
    retry: 1,
  });

  useEffect(() => {
    if (!selected) {
      return;
    }
    setProfileName(selected.profile?.name ?? "default");
    setBaseUrl(selected.profile?.base_url ?? selected.config.base_url ?? "http://127.0.0.1:8080");
    setModel(selected.profile?.virtual_model_id ?? routes.data?.[0]?.virtual_model_id ?? "");
    setKeyId(selected.profile?.virtual_key_id ?? "");
    // Sync saved profile values without discarding an in-progress preview.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected?.id, selected?.profile?.updated_at, routes.data?.[0]?.virtual_model_id]);

  useEffect(() => {
    setPlaintextKey("");
    setPreview(null);
    setNotice(null);
  }, [selected?.id]);

  function selectAgent(id: string) {
    setSelectedId(id);
  }

  function invalidateAgent(id: string) {
    void queryClient.invalidateQueries({ queryKey: ["agents"] });
    void queryClient.invalidateQueries({ queryKey: ["agent-backups", id] });
  }

  const saveProfile = useMutation({
    mutationFn: async () => {
      if (!selected) {
        throw new Error("select an agent first");
      }
      const body = {
        name: profileName.trim() || "default",
        base_url: baseUrl.trim(),
        virtual_model_id: model.trim(),
        virtual_key_id: keyId || null,
      };
      return selected.profile
        ? controlClient().updateAgentProfile(selected.id, selected.profile.id, body)
        : controlClient().createAgentProfile(selected.id, body);
    },
    onSuccess: () => {
      if (selected) {
        invalidateAgent(selected.id);
      }
      setNotice("Profile saved.");
    },
    onError: (error) => setNotice(errorMessage(error)),
  });

  const generateKey = useMutation({
    mutationFn: async () => {
      if (!selected) {
        throw new Error("select an agent first");
      }
      return controlClient().createLlmVirtualKey({
        name: `${selected.id}-${new Date().toISOString().slice(0, 10)}`,
      });
    },
    onSuccess: (created) => {
      setPlaintextKey(created.key_value);
      setKeyId(created.id);
      void queryClient.invalidateQueries({ queryKey: ["llm-virtual-keys"] });
      setNotice(
        `New key ${created.key_prefix}… created. Apply now writes it into the agent config; it cannot be retrieved again.`,
      );
    },
    onError: (error) => setNotice(errorMessage(error)),
  });

  const runPreview = useMutation({
    mutationFn: async () => {
      if (!selected) {
        throw new Error("select an agent first");
      }
      return controlClient().previewAgent(selected.id, {
        base_url: baseUrl.trim(),
        virtual_model_id: model.trim(),
        virtual_key: plaintextKey.trim() || undefined,
        config_path: selected.profile?.config_path,
      });
    },
    onSuccess: (result) => {
      setPreview(result);
      setNotice(null);
    },
    onError: (error) => setNotice(errorMessage(error)),
  });

  const applyConfig = useMutation({
    mutationFn: async () => {
      if (!selected) {
        throw new Error("select an agent first");
      }
      if (!plaintextKey.trim()) {
        throw new Error("paste or generate a virtual key before applying");
      }
      return controlClient().applyAgent(selected.id, {
        base_url: baseUrl.trim(),
        virtual_model_id: model.trim(),
        virtual_key: plaintextKey.trim(),
        config_path: selected.profile?.config_path,
        expected_sha256: preview?.current_sha256 ?? undefined,
      });
    },
    onSuccess: (result) => {
      if (selected) {
        invalidateAgent(selected.id);
      }
      setPlaintextKey("");
      setPreview(null);
      setNotice(
        `Configuration applied to ${result.path}. The previous file is backed up as ${result.backup_id.slice(0, 8)}.`,
      );
    },
    onError: (error) => setNotice(errorMessage(error)),
  });

  const restore = useMutation({
    mutationFn: async () => {
      if (!selected) {
        throw new Error("select an agent first");
      }
      return controlClient().restoreAgent(selected.id);
    },
    onSuccess: (result) => {
      if (selected) {
        invalidateAgent(selected.id);
      }
      setPreview(null);
      setNotice(
        result.removed
          ? "Restored: the configuration file Octra created was removed."
          : `Restored the previous configuration from ${result.backup_id.slice(0, 8)}.`,
      );
    },
    onError: (error) => setNotice(errorMessage(error)),
  });

  const installed = agents.data?.filter((agent) => agent.detection.installed).length ?? 0;
  const configured = agents.data?.filter((agent) => agent.configured).length ?? 0;
  const profiles = agents.data?.filter((agent) => agent.profile !== null).length ?? 0;
  const attention = agents.data?.reduce((count, agent) => count + agent.warnings.length, 0) ?? 0;

  const stats: StatItem[] = [
    {
      label: "Detected",
      value: `${installed} / ${agents.data?.length ?? 0}`,
      meta: "CLI found on PATH",
      tone: "primary",
    },
    {
      label: "Configured",
      value: String(configured),
      meta: "pointing at this gateway",
      tone: "primary",
    },
    { label: "Profiles", value: String(profiles), meta: "saved bindings", tone: "violet" },
    {
      label: "Attention",
      value: String(attention),
      meta: "configuration warnings",
      tone: attention > 0 ? "coral" : "muted",
    },
  ];

  const busy =
    saveProfile.isPending ||
    generateKey.isPending ||
    runPreview.isPending ||
    applyConfig.isPending ||
    restore.isPending;
  const canApply =
    selected !== null &&
    plaintextKey.trim().length > 0 &&
    model.trim().length > 0 &&
    baseUrl.trim().length > 0;

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Control / agents"
        title="Agents"
        description="Point local coding agents at this gateway. Octra writes their provider settings; it never proxies their traffic."
        actions={
          <>
            <Button
              variant="secondary"
              onClick={() => {
                void queryClient.invalidateQueries({ queryKey: ["agents"] });
                void queryClient.invalidateQueries({ queryKey: ["agent-backups"] });
              }}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              Rescan
            </Button>
            <Button
              onClick={() => saveProfile.mutate()}
              disabled={!selected || saveProfile.isPending}
            >
              <Save className="h-3.5 w-3.5" />
              Save profile
            </Button>
          </>
        }
      />

      {mode.data?.sandbox ? (
        <div className="flex flex-wrap items-center gap-2 rounded-lg border border-amber/40 bg-amber/5 px-4 py-2.5 text-[11px] text-amber">
          <ShieldCheck className="h-3.5 w-3.5 shrink-0" strokeWidth={1.5} />
          <span>
            Sandbox mode. Agent configs are read from and written to{" "}
            <span className="font-mono">{mode.data.home}</span>; your real agent files are
            untouched.
          </span>
        </div>
      ) : null}

      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <Bot className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
                <CardTitle>Installed agents</CardTitle>
              </div>
              <CardDescription>Detection checks PATH and the standard config path</CardDescription>
            </div>
            <Badge variant="accent">{agents.data?.length ?? 0} supported</Badge>
          </CardHeader>
          <CardSeparator />
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Agent</TableHead>
                <TableHead>Binary</TableHead>
                <TableHead>Config</TableHead>
                <TableHead>Endpoint</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="text-right">Action</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(agents.data ?? []).map((agent) => (
                <TableRow
                  key={agent.id}
                  className={cn("cursor-pointer", selected?.id === agent.id && "bg-surface")}
                  onClick={() => selectAgent(agent.id)}
                >
                  <TableCell className="h-16">
                    <div className="flex items-center gap-2.5">
                      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-raised">
                        <Bot className="h-3.5 w-3.5 text-secondary" strokeWidth={1.5} />
                      </span>
                      <div className="flex flex-col">
                        <span className="font-mono text-[11.5px]">{agent.name}</span>
                        <span className="text-[10px] text-muted-foreground">{agent.vendor}</span>
                      </div>
                    </div>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-[10.5px] text-secondary">
                      {agent.detection.installed
                        ? compactPath(agent.detection.binary)
                        : "not found"}
                    </span>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-[10.5px] text-muted-foreground">
                      {agent.config.exists
                        ? compactPath(agent.detection.config_path)
                        : "not created"}
                    </span>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-[10.5px] text-secondary">
                      {agent.config.base_url ?? "—"}
                    </span>
                  </TableCell>
                  <TableCell className="h-16">
                    <AgentStatus agent={agent} />
                  </TableCell>
                  <TableCell className="h-16 text-right">
                    <Button
                      size="sm"
                      variant={selected?.id === agent.id ? "default" : "secondary"}
                      onClick={(event) => {
                        event.stopPropagation();
                        selectAgent(agent.id);
                      }}
                    >
                      Configure
                      <ArrowRight className="h-3.5 w-3.5" />
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {agents.isPending ? (
            <p className="px-5 py-4 text-xs text-muted-foreground">Detecting local agents…</p>
          ) : null}
          {agents.isError ? (
            <p className="px-5 py-4 text-xs text-danger">{errorMessage(agents.error)}</p>
          ) : null}
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {agents.data?.length ?? 0} adapters · config files stay on this machine
            </span>
            <span className="font-mono text-[10.5px] text-muted-foreground">
              atomic write · 0600 · backup first
            </span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex min-w-0 flex-col gap-1">
              <CardTitle>{selected?.name ?? "Select an agent"}</CardTitle>
              <CardDescription>
                {selected?.detection.config_path ?? "Choose a row to configure it"}
              </CardDescription>
            </div>
            {selected ? <AgentStatus agent={selected} /> : null}
          </CardHeader>
          <CardSeparator />
          {selected ? (
            <div className="flex flex-col gap-3 p-4">
              <Field label="Profile name">
                <Input
                  value={profileName}
                  onChange={(event) => setProfileName(event.target.value)}
                  placeholder="default"
                />
              </Field>
              <Field label="Gateway URL">
                <Input
                  value={baseUrl}
                  onChange={(event) => setBaseUrl(event.target.value)}
                  placeholder="http://127.0.0.1:8080"
                  className="font-mono text-[11px]"
                />
              </Field>
              <Field label="Virtual model">
                {routes.data && routes.data.length > 0 ? (
                  <select
                    value={model}
                    onChange={(event) => setModel(event.target.value)}
                    className="h-8 w-full rounded-md border border-border bg-surface px-2 font-mono text-[11px] text-foreground"
                  >
                    <option value="">Select a model route…</option>
                    {routes.data.map((route) => (
                      <option key={route.id} value={route.virtual_model_id}>
                        {route.virtual_model_id}
                      </option>
                    ))}
                  </select>
                ) : (
                  <Input
                    value={model}
                    onChange={(event) => setModel(event.target.value)}
                    placeholder="octra-default"
                    className="font-mono text-[11px]"
                  />
                )}
              </Field>
              <Field
                label="Virtual key"
                hint="The plaintext key is written into the agent config and never read back."
              >
                <div className="flex gap-2">
                  <select
                    value={keyId}
                    onChange={(event) => setKeyId(event.target.value)}
                    className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-[10.5px] text-foreground"
                  >
                    <option value="">New or pasted key</option>
                    {(keys.data ?? []).map((key) => (
                      <option key={key.id} value={key.id}>
                        {key.name} · {key.key_prefix}…
                      </option>
                    ))}
                  </select>
                  <Button
                    size="sm"
                    variant="secondary"
                    onClick={() => generateKey.mutate()}
                    disabled={busy}
                  >
                    <KeyRound className="h-3.5 w-3.5" />
                    Generate
                  </Button>
                </div>
                <Input
                  type="password"
                  value={plaintextKey}
                  onChange={(event) => setPlaintextKey(event.target.value)}
                  placeholder="octra-…"
                  className="mt-2 font-mono text-[11px]"
                />
              </Field>

              {selected.warnings.length > 0 ? (
                <ul className="flex flex-col gap-1 rounded-lg border border-warning/40 bg-warning/5 p-2.5">
                  {selected.warnings.map((warning) => (
                    <li key={warning} className="text-[10.5px] text-warning">
                      {warning}
                    </li>
                  ))}
                </ul>
              ) : null}

              {preview ? <PreviewChanges preview={preview} /> : null}

              <div className="flex flex-wrap gap-2 pt-1">
                <Button
                  variant="secondary"
                  onClick={() => runPreview.mutate()}
                  disabled={busy || !model.trim()}
                >
                  <Sparkles className="h-3.5 w-3.5" />
                  Preview
                </Button>
                <Button onClick={() => applyConfig.mutate()} disabled={busy || !canApply}>
                  <CheckCircle2 className="h-3.5 w-3.5" />
                  Apply
                </Button>
                <Button
                  variant="ghost"
                  onClick={() => restore.mutate()}
                  disabled={busy || !selected.last_backup}
                >
                  <RotateCcw className="h-3.5 w-3.5" />
                  Restore
                </Button>
              </div>

              {notice ? (
                <p
                  className={cn(
                    "text-[10.5px]",
                    /fail|error|cannot|invalid|conflict/i.test(notice)
                      ? "text-danger"
                      : "text-secondary",
                  )}
                >
                  {notice}
                </p>
              ) : null}

              <div className="mt-1 flex items-center gap-2 rounded-lg border border-border bg-surface p-2.5 text-[10.5px] text-muted-foreground">
                <ShieldCheck className="h-3.5 w-3.5 shrink-0" strokeWidth={1.5} />
                <span>
                  Every apply records the original file first, then writes atomically with 0600
                  permissions.
                </span>
              </div>

              {backups.data && backups.data.length > 0 ? (
                <div className="flex flex-col gap-1 border-t border-border pt-2">
                  <span className="text-[10.5px] text-muted-foreground">Recent backups</span>
                  {backups.data.slice(0, 3).map((backup) => (
                    <div
                      key={backup.id}
                      className="flex items-center justify-between font-mono text-[10px] text-secondary"
                    >
                      <span>{backup.id.slice(0, 8)}</span>
                      <span>{formatRelativeTime(backup.created_at)}</span>
                      <span
                        className={backup.restored_at ? "text-muted-foreground" : "text-primary"}
                      >
                        {backup.restored_at ? "restored" : "available"}
                      </span>
                    </div>
                  ))}
                </div>
              ) : null}
            </div>
          ) : (
            <div className="p-5 text-xs text-muted-foreground">
              Select an agent to preview and apply its configuration.
            </div>
          )}
        </Card>
      </div>
    </div>
  );
}

function AgentStatus({ agent }: { agent: AgentView }) {
  const status = !agent.detection.installed
    ? { label: "Not installed", tone: "muted" as const }
    : agent.configured
      ? { label: "Connected", tone: "primary" as const }
      : { label: "Needs config", tone: "amber" as const };

  return (
    <span className="flex items-center gap-1.5">
      <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[status.tone])} />
      <span className={cn("text-[11px]", toneText[status.tone])}>{status.label}</span>
    </span>
  );
}

function PreviewChanges({ preview }: { preview: AgentPreview }) {
  return (
    <div className="rounded-lg border border-border bg-surface p-2.5">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[10.5px] font-medium text-foreground">Proposed changes</span>
        <span className="font-mono text-[9.5px] text-muted-foreground">
          {preview.exists ? "merge" : "new file"}
        </span>
      </div>
      {preview.changes.length === 0 ? (
        <p className="mt-1.5 text-[10.5px] text-muted-foreground">
          The configuration already matches this profile.
        </p>
      ) : (
        <ul className="mt-1.5 flex flex-col gap-1">
          {preview.changes.map((change) => (
            <li
              key={change.field}
              className="flex flex-col gap-0.5 border-b border-border pb-1 last:border-0"
            >
              <span className="font-mono text-[10px] text-secondary">{change.field}</span>
              <span className="truncate font-mono text-[10px] text-muted-foreground">
                {change.before ?? "(unset)"} → {change.after ?? "(removed)"}
              </span>
            </li>
          ))}
        </ul>
      )}
      {preview.warnings.length > 0 ? (
        <ul className="mt-1.5 flex flex-col gap-1">
          {preview.warnings.map((warning) => (
            <li key={warning} className="text-[10px] text-warning">
              {warning}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[10.5px] text-muted-foreground">{label}</span>
      {children}
      {hint ? <span className="text-[9.5px] text-muted-foreground">{hint}</span> : null}
    </label>
  );
}

function compactPath(path: string | null | undefined): string {
  if (!path) {
    return "—";
  }
  const parts = path.split("/");
  return parts.length <= 2 ? path : `…/${parts.slice(-2).join("/")}`;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "request failed";
}
