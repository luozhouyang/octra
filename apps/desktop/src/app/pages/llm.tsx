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
import { Plus, Receipt } from "lucide-react";

import { PageHeader } from "../../components/page-header";
import { StatStrip, type StatItem } from "../../components/stat-strip";
import { modelRoutes, providerMix } from "../../lib/sample-data";
import { toneBar, toneDot } from "../../lib/tone";

const stats: StatItem[] = [
  { label: "Requests", value: "8,412", meta: "44.4% of traffic", tone: "primary" },
  { label: "Tokens", value: "14.2M", meta: "in · 3.1M out", tone: "violet" },
  { label: "Spend", value: "$38.40", meta: "of $50.00 budget", tone: "amber" },
  { label: "p95 latency", value: "890 ms", meta: "claude-sonnet-5.5", tone: "sky" },
];

export function LlmPage() {
  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Runtime / LLM"
        title="LLM Gateway"
        description="Model routes, provider fallbacks and token accounting for every completion."
        actions={
          <>
            <Button variant="secondary">
              <Receipt className="h-3.5 w-3.5" />
              Pricing table
            </Button>
            <Button>
              <Plus className="h-3.5 w-3.5" />
              New model route
            </Button>
          </>
        }
      />

      <StatStrip items={stats} />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Model routes</CardTitle>
              <CardDescription>Fallback order is evaluated top to bottom</CardDescription>
            </div>
          </CardHeader>
          <CardSeparator />
          <div className="flex-1">
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Model</TableHead>
                  <TableHead>Provider</TableHead>
                  <TableHead className="text-right">Requests</TableHead>
                  <TableHead className="text-right">Tokens</TableHead>
                  <TableHead className="text-right">Spend</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {modelRoutes.map((route) => (
                  <TableRow key={route.model}>
                    <TableCell>
                      <div className="flex items-center gap-2">
                        <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[route.tone])} />
                        <span className="font-mono text-[11.5px]">{route.model}</span>
                      </div>
                    </TableCell>
                    <TableCell className="text-[11.5px] text-secondary">{route.provider}</TableCell>
                    <TableCell className="text-right font-mono text-[12px]">
                      {route.requests}
                    </TableCell>
                    <TableCell className="text-right font-mono text-[12px] text-muted-foreground">
                      {route.tokens}
                    </TableCell>
                    <TableCell className="text-right font-mono text-[12px] text-foreground">
                      {route.spend}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">8 model routes · 4 providers</span>
            <span className="font-mono text-[11px] text-muted-foreground">
              fallback: gpt-4.1 → claude-sonnet-5.5
            </span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader>
            <div className="flex flex-col gap-1">
              <CardTitle>Provider mix</CardTitle>
              <CardDescription>Requests by provider · last 24 hours</CardDescription>
            </div>
            <Badge variant="accent">8,412</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="space-y-4 p-4">
            {providerMix.map((provider) => (
              <div key={provider.name} className="space-y-1.5">
                <div className="flex items-center gap-2">
                  <span className={cn("h-1.5 w-1.5 rounded-full", toneDot[provider.tone])} />
                  <span className="text-[11.5px] text-secondary">{provider.name}</span>
                  <span className="ml-auto font-mono text-[11px] text-secondary">
                    {provider.share}
                  </span>
                </div>
                <div className="h-1.5 w-full overflow-hidden rounded-full bg-raised">
                  <div
                    className={cn("h-full rounded-full", toneBar[provider.tone])}
                    style={{ width: `${provider.percent}%` }}
                  />
                </div>
                <p className="text-[10.5px] text-muted-foreground">{provider.meta}</p>
              </div>
            ))}
          </div>
          <div className="flex-1" />
          <CardSeparator />
          <div className="p-4">
            <div className="flex items-baseline justify-between gap-3">
              <span className="text-[11px] text-muted-foreground">Budget guard</span>
              <span className="font-mono text-[11.5px]">$38.40 / $50.00</span>
            </div>
            <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-raised">
              <div className="h-full rounded-full bg-amber" style={{ width: "77%" }} />
            </div>
            <div className="mt-2.5 flex items-center justify-between text-[10.5px]">
              <span className="text-muted-foreground">Resets in 9h 46m</span>
              <span className="text-secondary">Manage budget</span>
            </div>
          </div>
        </Card>
      </div>
    </div>
  );
}
