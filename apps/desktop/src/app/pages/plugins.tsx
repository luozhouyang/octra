import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Badge,
  Card,
  CardDescription,
  CardFooter,
  CardHeader,
  CardSeparator,
  CardTitle,
  Switch,
  cn,
} from "@octra/ui";

import { PageHeader } from "../../components/page-header";
import { StatStrip } from "../../components/stat-strip";
import { controlClient } from "../../lib/control-api";
import { formatNumber } from "../../lib/format";
import { toneDot, toneText, type Tone } from "../../lib/tone";

const stageOrder = [
  "request",
  "provider_request",
  "response",
  "final_response",
  "stream_event",
  "error",
  "log",
] as const;

function formatLatency(calls: number, micros: number): string {
  return calls === 0 ? "—" : `${(micros / 1000).toFixed(1)} ms`;
}

export function PluginsPage() {
  const queryClient = useQueryClient();

  const pluginsQuery = useQuery({
    queryKey: ["llm-plugins"],
    queryFn: () => controlClient().llmPlugins(),
    retry: 1,
  });

  const metricsQuery = useQuery({
    queryKey: ["llm-plugin-metrics"],
    queryFn: () => controlClient().llmPluginMetrics(),
    retry: 1,
  });

  const togglePlugin = useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      controlClient().setLlmPluginEnabled(id, enabled),
    onSettled: () => {
      void queryClient.invalidateQueries({ queryKey: ["llm-plugins"] });
      void queryClient.invalidateQueries({ queryKey: ["llm-plugin-metrics"] });
    },
  });

  const plugins = pluginsQuery.data ?? [];
  const metricsEntries = metricsQuery.data?.plugins ?? [];
  const orderedPlugins = [...plugins].sort((a, b) => b.priority - a.priority);

  const enabledCount = plugins.filter((plugin) => plugin.enabled).length;
  const requestCalls = metricsEntries.reduce(
    (total, entry) => total + entry.stages.request.calls,
    0,
  );
  const requestRejections = metricsEntries.reduce(
    (total, entry) => total + entry.stages.request.rejections,
    0,
  );

  const stats = [
    {
      label: "Enabled",
      value: String(enabledCount),
      meta: `of ${plugins.length} plugins`,
      tone: "primary" as Tone,
    },
    {
      label: "Instances",
      value: String(metricsEntries.length),
      meta: "configured scopes",
      tone: "sky" as Tone,
    },
    {
      label: "Calls",
      value: formatNumber(requestCalls),
      meta: "request stage",
      tone: "violet" as Tone,
    },
    {
      label: "Blocked",
      value: formatNumber(requestRejections),
      meta: "rejections",
      tone: "coral" as Tone,
    },
  ];

  const stageRows = stageOrder.map((stage) => {
    let calls = 0;
    let rejections = 0;
    let p95 = 0;

    for (const entry of metricsEntries) {
      const stageMetrics = entry.stages[stage];
      calls += stageMetrics.calls;
      rejections += stageMetrics.rejections;
      p95 = Math.max(p95, stageMetrics.latency_p95_micros);
    }

    return { stage, calls, rejections, p95 };
  });
  const totalRejections = stageRows.reduce((total, row) => total + row.rejections, 0);

  function latencyFor(pluginName: string): string {
    const entries = metricsEntries.filter((entry) => entry.name === pluginName);
    const calls = entries.reduce((total, entry) => total + entry.stages.request.calls, 0);
    const p95 = entries.reduce(
      (max, entry) => Math.max(max, entry.stages.request.latency_p95_micros),
      0,
    );

    return formatLatency(calls, p95);
  }

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Control / policy"
        title="Plugins"
        description="Request hooks and policy stages evaluated in order for every call."
      />

      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Pipeline</CardTitle>
              <CardDescription>Sorted by priority; higher values execute first</CardDescription>
            </div>
          </CardHeader>
          <CardSeparator />
          <ul>
            {pluginsQuery.isPending ? (
              <li className="flex h-14 items-center px-5 text-[11px] text-muted-foreground">
                Loading plugins…
              </li>
            ) : pluginsQuery.isError ? (
              <li className="flex h-14 items-center px-5 text-[11px] text-danger">
                Unable to load plugins.
              </li>
            ) : orderedPlugins.length === 0 ? (
              <li className="flex h-14 items-center px-5 text-[11px] text-muted-foreground">
                No LLM plugins are registered.
              </li>
            ) : (
              orderedPlugins.map((plugin, index) => {
                const pending = togglePlugin.isPending && togglePlugin.variables?.id === plugin.id;

                return (
                  <li
                    key={plugin.id}
                    className="flex h-14 items-center gap-4 border-b border-border px-5 last:border-0"
                  >
                    <span className="w-6 font-mono text-[10px] text-muted-foreground">
                      {String(index + 1).padStart(2, "0")}
                    </span>
                    <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                      <span className="font-mono text-[11.5px] text-foreground">{plugin.name}</span>
                      <span className="truncate text-[10px] text-muted-foreground">
                        {plugin.description}
                      </span>
                    </div>
                    <span className="w-16 font-mono text-[10.5px] text-muted-foreground">
                      v{plugin.version}
                    </span>
                    <span
                      className="w-16 font-mono text-[10.5px] text-muted-foreground"
                      title="Execution priority"
                    >
                      prio {plugin.priority}
                    </span>
                    <span className="w-16 font-mono text-[10.5px] text-secondary">
                      {latencyFor(plugin.name)}
                    </span>
                    <Switch
                      checked={plugin.enabled}
                      onCheckedChange={(checked) =>
                        togglePlugin.mutate({ id: plugin.id, enabled: checked })
                      }
                      disabled={pending}
                      label={`${plugin.name} enabled`}
                    />
                  </li>
                );
              })
            )}
          </ul>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {enabledCount} of {plugins.length} plugins enabled
            </span>
            <span className="font-mono text-[11px] text-secondary">wasm · v1 hooks</span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Stage latency</CardTitle>
              <CardDescription>P95 across configured scopes</CardDescription>
            </div>
            <Badge variant={totalRejections > 0 ? "danger" : "neutral"}>
              {formatNumber(totalRejections)} blocked
            </Badge>
          </CardHeader>
          <CardSeparator />
          <div className="p-4">
            {metricsQuery.isPending ? (
              <div className="flex h-16 items-center text-[11px] text-muted-foreground">
                Loading metrics…
              </div>
            ) : metricsQuery.isError ? (
              <div className="flex h-16 items-center text-[11px] text-muted-foreground">
                Plugin metrics are unavailable.
              </div>
            ) : metricsEntries.length === 0 ? (
              <div className="flex h-16 items-center text-[11px] text-muted-foreground">
                No plugin traffic recorded yet.
              </div>
            ) : (
              stageRows.map((row) => (
                <div
                  key={row.stage}
                  className="flex h-12 items-center gap-3 border-b border-border last:border-0"
                >
                  <span
                    className={cn(
                      "h-1.5 w-1.5 rounded-full",
                      row.rejections > 0
                        ? toneDot.danger
                        : row.calls > 0
                          ? toneDot.primary
                          : toneDot.muted,
                    )}
                  />
                  <span className="flex-1 font-mono text-[11.5px] text-foreground">
                    {row.stage}
                  </span>
                  <span className="font-mono text-[10px] text-muted-foreground">
                    {formatNumber(row.calls)} calls
                  </span>
                  <span
                    className={cn(
                      "w-16 text-right font-mono text-[10.5px]",
                      row.rejections > 0 ? toneText.danger : "text-secondary",
                    )}
                  >
                    {formatLatency(row.calls, row.p95)}
                  </span>
                </div>
              ))
            )}
          </div>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {metricsEntries.length} configured instances · hook contract v1
            </span>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}
