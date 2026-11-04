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
import { ArrowDownRight, ArrowRight, Bot, Download, Plus } from "lucide-react";
import { useState } from "react";

import { PageHeader } from "../../components/page-header";
import { StatStrip } from "../../components/stat-strip";
import { agentSessions, agentTrace } from "../../lib/sample-data";
import { toneDot, toneText } from "../../lib/tone";

type SessionFilter = "all" | "running" | "failed";

export function AgentsPage() {
  const [filter, setFilter] = useState<SessionFilter>("all");

  const sessions = agentSessions.filter((session) => {
    if (filter === "running") return session.status === "Running";
    if (filter === "failed") return session.status === "Failed";
    return true;
  });
  const runningCount = agentSessions.filter((session) => session.status === "Running").length;

  return (
    <div className="flex flex-col gap-5">
      <PageHeader
        eyebrow="Control / sessions"
        title="Agents"
        description="Agent sessions, tool calls and step traces running on this daemon."
        actions={
          <>
            <Button variant="secondary">
              <Download className="h-3.5 w-3.5" />
              Export trace
            </Button>
            <Button>
              <Plus className="h-3.5 w-3.5" />
              New session
            </Button>
          </>
        }
      />

      <StatStrip
        items={[
          { label: "Sessions", value: "6", meta: "2 running now", tone: "primary" },
          { label: "Steps", value: "148", meta: "avg 24 per session", tone: "violet" },
          { label: "Tool calls", value: "412", meta: "across 4 servers", tone: "amber" },
          { label: "Success", value: "92.4%", meta: "57 of 62 steps", tone: "sky" },
        ]}
      />

      <div className="grid gap-4 xl:grid-cols-3">
        <Card className="flex flex-col xl:col-span-2">
          <CardHeader className="items-center">
            <div className="flex min-w-0 flex-col gap-1">
              <CardTitle>Sessions</CardTitle>
              <CardDescription>
                Bounded runs with a step budget and a tool allowlist
              </CardDescription>
            </div>
            <Segmented
              value={filter}
              onChange={setFilter}
              options={[
                { value: "all", label: "All" },
                { value: "running", label: "Running" },
                { value: "failed", label: "Failed" },
              ]}
            />
          </CardHeader>
          <CardSeparator />
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>Agent</TableHead>
                <TableHead>Model</TableHead>
                <TableHead>Steps</TableHead>
                <TableHead>Duration</TableHead>
                <TableHead>Status</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {sessions.map((session) => (
                <TableRow key={session.agent}>
                  <TableCell className="h-16">
                    <div className="flex items-center gap-2.5">
                      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-raised">
                        <Bot className="h-3.5 w-3.5 text-muted-foreground" strokeWidth={1.5} />
                      </span>
                      <span className="font-mono text-xs">{session.agent}</span>
                    </div>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-[11px] text-secondary">{session.model}</span>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-xs">{session.steps}</span>
                  </TableCell>
                  <TableCell className="h-16">
                    <span className="font-mono text-xs text-muted-foreground">
                      {session.duration}
                    </span>
                  </TableCell>
                  <TableCell className="h-16">
                    <div className="flex items-center gap-2">
                      <span className={cn("h-[5px] w-[5px] rounded-full", toneDot[session.tone])} />
                      <span className={cn("text-xs", toneText[session.tone])}>
                        {session.status}
                      </span>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">
              {agentSessions.length} sessions · {runningCount} running
            </span>
            <span className="font-mono text-[11px] text-muted-foreground">step budget: 50</span>
          </CardFooter>
        </Card>

        <Card className="flex flex-col">
          <CardHeader className="items-center">
            <div className="flex min-w-0 flex-col gap-1">
              <CardTitle>Live trace</CardTitle>
              <CardDescription className="font-mono">repo-audit · session 4f2c</CardDescription>
            </div>
            <Badge variant="violet">Running</Badge>
          </CardHeader>
          <CardSeparator />
          <div className="p-4">
            {agentTrace.map((step) => (
              <div
                key={step.index}
                className="flex h-16 items-center gap-3 border-b border-border last:border-0"
              >
                <span className="w-6 shrink-0 font-mono text-[10px] text-muted-foreground">
                  {step.index}
                </span>
                <div className="flex min-w-0 flex-col gap-0.5">
                  <span className="truncate font-mono text-[11.5px]">{step.tool}</span>
                  <span className="text-[10px] text-muted-foreground">{step.server}</span>
                </div>
                <div className="ml-auto flex flex-col items-end gap-1">
                  <span
                    className={cn(
                      "font-mono text-[10.5px]",
                      step.state === "running" ? "text-violet" : "text-secondary",
                    )}
                  >
                    {step.latency}
                  </span>
                  <span className="flex items-center gap-1.5">
                    <span
                      className={cn(
                        "h-[4px] w-[4px] rounded-full",
                        step.state === "running" ? "bg-violet" : "bg-primary",
                      )}
                    />
                    <span className="text-[9.5px] text-muted-foreground">{step.state}</span>
                  </span>
                </div>
              </div>
            ))}
            <div className="flex items-center gap-2 pt-3 text-[11px] text-muted-foreground">
              <ArrowDownRight className="h-3.5 w-3.5" strokeWidth={1.5} />
              Next: summarize findings
            </div>
          </div>
          <div className="flex-1" />
          <CardFooter>
            <span className="text-[11px] text-muted-foreground">Started 7m 48s ago</span>
            <span className="flex items-center gap-1.5 text-[11px] text-secondary">
              Follow live
              <ArrowRight className="h-3.5 w-3.5 text-muted-foreground" strokeWidth={1.5} />
            </span>
          </CardFooter>
        </Card>
      </div>
    </div>
  );
}
