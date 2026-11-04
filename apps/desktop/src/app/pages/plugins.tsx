import {
  Badge,
  Button,
  Card,
  CardDescription,
  CardFooter,
  CardHeader,
  CardSeparator,
  CardTitle,
  Switch,
  cn,
} from "@octra/ui";
import { ArrowRight, BookOpen, GripVertical, Plus } from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip } from "../../components/stat-strip";
import { pluginDecisions, pluginStages } from "../../lib/sample-data";
import { toneDot, toneText, type Tone } from "../../lib/tone";

const stats = [
  { label: "Enabled", value: "8", meta: "of 9 stages", tone: "primary" as Tone },
  { label: "Stages", value: "9", meta: "request and response", tone: "violet" as Tone },
  { label: "Overhead", value: "4.2 ms", meta: "avg per request", tone: "amber" as Tone },
  { label: "Blocked", value: "38", meta: "in the last 24h", tone: "coral" as Tone },
];

export function PluginsPage() {
  const [enabled, setEnabled] = useState<Record<string, boolean>>(() =>
    Object.fromEntries(pluginStages.map((stage) => [stage.index, stage.enabled])),
  );

  const enabledCount = pluginStages.filter((stage) => enabled[stage.index]).length;

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Control / policy"
        title="Plugins"
        description="Request hooks and policy stages evaluated in order for every call."
        actions={
          <>
            <Button variant="secondary">
              <BookOpen className="h-3.5 w-3.5" />
              Plugin docs
            </Button>
            <Button>
              <Plus className="h-3.5 w-3.5" />
              Add plugin
            </Button>
          </>
        }
      />

      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Pipeline</CardTitle>
              <CardDescription>
                Runs top to bottom; response hooks unwind in reverse
              </CardDescription>
            </div>
            <span className="flex items-center gap-1 text-[11px] text-muted-foreground">
              <GripVertical className="h-3.5 w-3.5" strokeWidth={1.5} />
              Reorder
            </span>
          </CardHeader>
          <CardSeparator />
          <ul>
            {pluginStages.map((stage) => (
              <li
                key={stage.index}
                className="flex h-14 items-center gap-4 border-b border-border px-5 last:border-0"
              >
                <span className="w-6 font-mono text-[10px] text-muted-foreground">
                  {stage.index}
                </span>
                <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                  <span className="font-mono text-[11.5px] text-foreground">{stage.name}</span>
                  <span className="text-[10px] text-muted-foreground">{stage.description}</span>
                </div>
                <span className="w-20 font-mono text-[10.5px] text-muted-foreground">
                  {stage.stage}
                </span>
                <span className="w-16 font-mono text-[10.5px] text-secondary">{stage.latency}</span>
                <Switch
                  checked={enabled[stage.index] ?? false}
                  onCheckedChange={(checked) =>
                    setEnabled((current) => ({ ...current, [stage.index]: checked }))
                  }
                  label={`${stage.name} enabled`}
                />
              </li>
            ))}
          </ul>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {enabledCount} of {pluginStages.length} stages enabled
            </span>
            <span className="font-mono text-[11px] text-secondary">wasm · v1 hooks</span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Recent decisions</CardTitle>
              <CardDescription>Every hook outcome · last 24 hours</CardDescription>
            </div>
            <Badge variant="danger">38 blocked</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="p-4">
            {pluginDecisions.map((decision) => (
              <div
                key={`${decision.time}-${decision.plugin}`}
                className="flex h-16 items-center gap-3 border-b border-border last:border-0"
              >
                <span className="font-mono text-[10px] text-muted-foreground">{decision.time}</span>
                <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                  <span className="font-mono text-[11.5px] text-foreground">{decision.plugin}</span>
                  <span className="text-[10px] text-muted-foreground">{decision.detail}</span>
                </div>
                <span
                  className={cn(
                    "flex items-center gap-1.5 text-[10px]",
                    toneText[decision.outcome === "allow" ? "primary" : "danger"],
                  )}
                >
                  <span
                    className={cn(
                      "h-1 w-1 rounded-full",
                      toneDot[decision.outcome === "allow" ? "primary" : "danger"],
                    )}
                  />
                  {decision.outcome}
                </span>
              </div>
            ))}
          </div>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">Hook contract v1</span>
            <span className="flex items-center gap-1.5 text-[11px] text-secondary">
              View logs <ArrowRight className="h-3.5 w-3.5 text-muted-foreground" />
            </span>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}
