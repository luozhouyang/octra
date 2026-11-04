import { Card, cn } from "@octra/ui";

import { toneDot, type Tone } from "../lib/tone";

export interface StatItem {
  label: string;
  value: string;
  meta: string;
  tone: Tone;
}

export function StatCard({ item }: { item: StatItem }) {
  return (
    <Card className="flex flex-col gap-1.5 p-4">
      <span className="text-[11.5px] text-muted-foreground">{item.label}</span>
      <div className="flex items-baseline gap-2">
        <span className="font-mono text-2xl font-medium leading-none">{item.value}</span>
        <span className={cn("h-1.5 w-1.5 rounded-full", toneDot[item.tone])} />
      </div>
      <span className="text-[10.5px] text-muted-foreground">{item.meta}</span>
    </Card>
  );
}

export function StatStrip({ items }: { items: StatItem[] }) {
  return (
    <div className="grid grid-cols-2 gap-4 xl:grid-cols-4">
      {items.map((item) => (
        <StatCard key={item.label} item={item} />
      ))}
    </div>
  );
}
