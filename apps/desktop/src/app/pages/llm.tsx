import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { AiProvider, ConsumerPolicy, LlmUsage } from "@octra/types";
import {
  Badge,
  Button,
  Card,
  CardDescription,
  CardFooter,
  CardHeader,
  CardSeparator,
  CardTitle,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  cn,
} from "@octra/ui";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip, type StatItem } from "../../components/stat-strip";
import { controlClient } from "../../lib/control-api";
import { formatCompact, formatNumber, formatRelativeTime } from "../../lib/format";
import { toneBar, toneDot, type Tone } from "../../lib/tone";
import { ConsumerPolicyForm, type ConsumerPolicyInput } from "./llm-forms";

const rowTones: Tone[] = ["primary", "sky", "violet", "amber", "coral"];

function toneAt(index: number): Tone {
  return rowTones[index % rowTones.length] ?? "muted";
}

function providerLabel(providers: AiProvider[], id: string): string {
  if (id.length === 0) {
    return "unassigned";
  }
  return providers.find((provider) => provider.id === id)?.name ?? id;
}

export function LlmPage() {
  const usage = useQuery({
    queryKey: ["llm-usage"],
    queryFn: () => controlClient().llmUsage(200),
    retry: 1,
  });

  const providersQuery = useQuery({
    queryKey: ["ai-providers"],
    queryFn: () => controlClient().aiProviders(),
    retry: 1,
  });

  const usageData = usage.data;
  const providers = providersQuery.data ?? [];
  const error = usage.error ?? providersQuery.error;
  const pending = usage.isPending || providersQuery.isPending;

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Runtime / LLM"
        title="LLM Gateway"
        description="Model routes, provider fallbacks and token accounting for every completion."
      />

      {error ? (
        <Card className="px-5 py-6 text-[11.5px] text-danger">
          {error instanceof Error ? error.message : String(error)}
        </Card>
      ) : pending ? (
        <Card className="px-5 py-6 text-[11.5px] text-muted-foreground">Loading usage…</Card>
      ) : usageData && usageData.totals.requests > 0 ? (
        <UsageOverview usage={usageData} providers={providers} />
      ) : (
        <Card className="px-5 py-6 text-[11.5px] text-muted-foreground">
          No LLM requests recorded yet.
        </Card>
      )}

      <ConsumerPolicyCard />
    </div>
  );
}

function UsageOverview({ usage, providers }: { usage: LlmUsage; providers: AiProvider[] }) {
  const { totals, by_model, by_provider, recent } = usage;
  const enabledProviders = providers.filter((provider) => provider.enabled).length;

  const latencySamples = recent
    .map((record) => record.latency_ms)
    .filter((value): value is number => value !== null);
  const latencyTotal = latencySamples.reduce((sum, value) => sum + value, 0);
  const averageLatency =
    latencySamples.length > 0 ? `${Math.round(latencyTotal / latencySamples.length)} ms` : "—";
  const lastRequest = recent[0]?.created_at ? formatRelativeTime(recent[0].created_at) : "—";

  const stats: StatItem[] = [
    {
      label: "Requests",
      value: formatNumber(totals.requests),
      meta: `${totals.errors} errors`,
      tone: "primary",
    },
    {
      label: "Tokens",
      value: `${formatCompact(totals.input_tokens)} in · ${formatCompact(totals.output_tokens)} out`,
      meta: `across ${by_model.length} models`,
      tone: "violet",
    },
    {
      label: "Cost",
      value: `$${(totals.cost_micros / 1_000_000).toFixed(2)}`,
      meta: `${by_model.length} models with usage`,
      tone: "amber",
    },
    {
      label: "Providers",
      value: `${providers.length}`,
      meta: `${enabledProviders} enabled`,
      tone: "sky",
    },
  ];

  return (
    <>
      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Model routes</CardTitle>
              <CardDescription>Aggregated usage per virtual model · all time</CardDescription>
            </div>
          </CardHeader>
          <CardSeparator />
          <div className="flex-1">
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Model</TableHead>
                  <TableHead className="text-right">Requests</TableHead>
                  <TableHead className="text-right">Tokens</TableHead>
                  <TableHead className="text-right">Errors</TableHead>
                  <TableHead className="text-right">Cost</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {by_model.map((row, index) => (
                  <TableRow key={row.virtual_model_id}>
                    <TableCell>
                      <div className="flex items-center gap-2">
                        <span
                          className={cn("h-[5px] w-[5px] rounded-full", toneDot[toneAt(index)])}
                        />
                        <span className="font-mono text-[11.5px]">{row.virtual_model_id}</span>
                      </div>
                    </TableCell>
                    <TableCell className="text-right font-mono text-[12px]">
                      {formatNumber(row.requests)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-[12px] text-muted-foreground">
                      {formatCompact(row.input_tokens + row.output_tokens)}
                    </TableCell>
                    <TableCell
                      className={cn(
                        "text-right font-mono text-[12px]",
                        row.errors > 0 ? "text-danger" : "text-muted-foreground",
                      )}
                    >
                      {formatNumber(row.errors)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-[12px] text-foreground">
                      ${(row.cost_micros / 1_000_000).toFixed(2)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {by_model.length} models with usage · {providers.length} providers
            </span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Provider mix</CardTitle>
              <CardDescription>Requests by provider · all time</CardDescription>
            </div>
            <Badge variant="accent">{formatNumber(totals.requests)}</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="space-y-4 p-4">
            {by_provider.map((provider, index) => {
              const share = totals.requests > 0 ? (provider.requests / totals.requests) * 100 : 0;
              const tone = toneAt(index);
              return (
                <div key={provider.provider_id} className="space-y-1.5">
                  <div className="flex items-center gap-2">
                    <span className={cn("h-1.5 w-1.5 rounded-full", toneDot[tone])} />
                    <span className="text-[11.5px] text-secondary">
                      {providerLabel(providers, provider.provider_id)}
                    </span>
                    <span className="ml-auto font-mono text-[11px] text-secondary">
                      {share.toFixed(1)}%
                    </span>
                  </div>
                  <div className="h-1.5 w-full overflow-hidden rounded-full bg-raised">
                    <div
                      className={cn("h-full rounded-full", toneBar[tone])}
                      style={{ width: `${share}%` }}
                    />
                  </div>
                  <p className="text-[10.5px] text-muted-foreground">
                    {formatNumber(provider.requests)} requests · $
                    {(provider.cost_micros / 1_000_000).toFixed(2)}
                  </p>
                </div>
              );
            })}
          </div>
          <div className="flex-1" />
          <CardSeparator />
          <div className="p-4">
            <div className="flex items-baseline justify-between gap-3">
              <span className="text-[11px] text-muted-foreground">Recent traffic</span>
              <span
                className={cn(
                  "font-mono text-[11.5px]",
                  totals.errors > 0 ? "text-danger" : "text-secondary",
                )}
              >
                {formatNumber(totals.errors)} errors
              </span>
            </div>
            <div className="mt-2.5 flex items-center justify-between gap-3 text-[10.5px]">
              <span className="text-muted-foreground">
                avg latency <span className="font-mono text-secondary">{averageLatency}</span>
              </span>
              <span className="text-muted-foreground">
                last request <span className="font-mono text-secondary">{lastRequest}</span>
              </span>
            </div>
          </div>
        </Card>
      </div>
    </>
  );
}

interface PolicyNotice {
  text: string;
  error: boolean;
}

function ConsumerPolicyCard() {
  const queryClient = useQueryClient();
  const [editingId, setEditingId] = useState<string | null>(null);
  const [formVersion, setFormVersion] = useState(0);
  const [notice, setNotice] = useState<PolicyNotice | null>(null);

  const policiesQuery = useQuery({
    queryKey: ["consumer-policies"],
    queryFn: () => controlClient().consumerPolicies(),
    retry: 1,
  });

  const policies = policiesQuery.data ?? [];
  const editing = policies.find((policy) => policy.id === editingId) ?? null;

  const save = useMutation({
    mutationFn: ({ id, body }: { id: string | null; body: ConsumerPolicyInput }) =>
      id
        ? controlClient().updateConsumerPolicy(id, body)
        : controlClient().createConsumerPolicy(body),
    onSuccess: (_result, variables) => {
      setNotice({ text: `Policy ${variables.id ? "updated" : "created"}.`, error: false });
      resetForm();
      void queryClient.invalidateQueries({ queryKey: ["consumer-policies"] });
    },
    onError: (error) => setNotice({ text: describeError(error), error: true }),
  });

  const remove = useMutation({
    mutationFn: (id: string) => controlClient().deleteConsumerPolicy(id),
    onSuccess: () => {
      setNotice({ text: "Policy deleted.", error: false });
      resetForm();
      void queryClient.invalidateQueries({ queryKey: ["consumer-policies"] });
    },
    onError: (error) => setNotice({ text: describeError(error), error: true }),
  });

  const busy = save.isPending || remove.isPending;

  function resetForm() {
    setEditingId(null);
    setFormVersion((version) => version + 1);
  }

  function startNew() {
    resetForm();
    setNotice(null);
  }

  function startEdit(policy: ConsumerPolicy) {
    setEditingId(policy.id);
    setFormVersion((version) => version + 1);
    setNotice(null);
  }

  function removePolicy(policy: ConsumerPolicy) {
    const confirmed = window.confirm(`Delete policy "${policy.name}"? This cannot be undone.`);
    if (confirmed) {
      remove.mutate(policy.id);
    }
  }

  return (
    <Card className="flex flex-col">
      <CardHeader className="flex-wrap gap-3">
        <div className="flex flex-col gap-1">
          <CardTitle>Consumer policies</CardTitle>
          <CardDescription>
            Model and provider allowlists enforced before an LLM request leaves the gateway.
          </CardDescription>
        </div>
        <div className="ml-auto flex items-center gap-2">
          <Badge variant="accent">{policies.length} policies</Badge>
          <Button size="sm" variant="secondary" disabled={busy} onClick={startNew}>
            <Plus className="h-3.5 w-3.5" />
            New policy
          </Button>
        </div>
      </CardHeader>
      <CardSeparator />
      <div className="grid gap-4 p-4 lg:grid-cols-[minmax(260px,1fr)_minmax(300px,1.1fr)]">
        <div className="min-w-0">
          <Table className="[&_td]:px-3 [&_th]:px-3">
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>Policy</TableHead>
                <TableHead>Scope</TableHead>
                <TableHead>Consumer</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {policiesQuery.isPending ? (
                <TableRow className="h-12">
                  <TableCell colSpan={4} className="text-center text-[11px] text-muted-foreground">
                    Loading policies…
                  </TableCell>
                </TableRow>
              ) : policiesQuery.isError ? (
                <TableRow className="h-12">
                  <TableCell colSpan={4} className="text-center text-[11px] text-muted-foreground">
                    Cannot load policies.{" "}
                    <span className="text-danger">{describeError(policiesQuery.error)}</span>
                  </TableCell>
                </TableRow>
              ) : policies.length === 0 ? (
                <TableRow className="h-12">
                  <TableCell colSpan={4} className="text-center text-[11px] text-muted-foreground">
                    No consumer policies yet.
                  </TableCell>
                </TableRow>
              ) : (
                policies.map((policy) => (
                  <TableRow key={policy.id} className="h-12">
                    <TableCell>
                      <div className="flex items-center gap-2">
                        <span
                          className={cn(
                            "h-[5px] w-[5px] rounded-full",
                            policy.id === editingId ? toneDot.primary : toneDot.muted,
                          )}
                        />
                        <span className="font-mono text-[11.5px]">{policy.name}</span>
                      </div>
                    </TableCell>
                    <TableCell>
                      <span className="font-mono text-[11px] text-secondary">{policy.scope}</span>
                    </TableCell>
                    <TableCell>
                      <span className="font-mono text-[11px] text-muted-foreground">
                        {policy.consumer_id ?? "—"}
                      </span>
                    </TableCell>
                    <TableCell className="text-right">
                      <div className="flex justify-end gap-1.5">
                        <Button
                          size="sm"
                          variant="secondary"
                          disabled={busy}
                          onClick={() => startEdit(policy)}
                        >
                          <Pencil className="h-3.5 w-3.5" />
                          Edit
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          className="text-danger hover:text-danger"
                          disabled={busy}
                          onClick={() => removePolicy(policy)}
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                          Delete
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>
        </div>
        <div className="flex min-w-0 flex-col gap-2">
          <ConsumerPolicyForm
            key={`${editingId ?? "new"}:${formVersion}`}
            initial={editing}
            saving={save.isPending}
            deleting={remove.isPending}
            onSave={(body) => save.mutate({ id: editingId, body })}
            onCancel={startNew}
            onDelete={editing ? () => removePolicy(editing) : undefined}
          />
          {notice ? (
            <span className={cn("text-[10.5px]", notice.error ? "text-danger" : "text-secondary")}>
              {notice.text}
            </span>
          ) : null}
        </div>
      </div>
      <CardFooter>
        <span className="text-[11px] text-muted-foreground">
          {policies.length} policies · global and consumer scope
        </span>
        <span className="font-mono text-[10.5px] text-muted-foreground">
          {editingId ? `editing ${editingId}` : "new policy"}
        </span>
      </CardFooter>
    </Card>
  );
}

function describeError(error: unknown): string {
  return error instanceof Error ? error.message : "request failed";
}
