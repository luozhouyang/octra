import { cn } from "@octra/ui";

export interface RequestChartProps {
  /** Normalized request volume, 0..1, evenly spaced. */
  values: number[];
  /** Normalized error rate, 0..1, same length as values. */
  errors?: number[];
  className?: string;
}

const W = 709;
const H = 170;
const TOP = 8;
const BOTTOM = 160;

function pointsFor(values: number[], floor = 0.08): Array<{ x: number; y: number }> {
  const n = values.length;
  return values.map((value, index) => {
    const x = n <= 1 ? 0 : (index / (n - 1)) * W;
    const clamped = Math.min(1, Math.max(floor, value));
    return { x, y: BOTTOM - clamped * (BOTTOM - TOP) };
  });
}

function smoothPath(points: Array<{ x: number; y: number }>): string {
  if (points.length === 0) {
    return "";
  }
  const [first, ...rest] = points;
  let path = `M${first?.x ?? 0} ${first?.y ?? 0}`;
  let previous = first!;
  for (const point of rest) {
    const midX = (previous.x + point.x) / 2;
    path += ` C${midX} ${previous.y} ${midX} ${point.y} ${point.x} ${point.y}`;
    previous = point;
  }
  return path;
}

export function RequestChart({ values, errors, className }: RequestChartProps) {
  const points = pointsFor(values);
  const line = smoothPath(points);
  const area = `${line} L${W} ${H} L0 ${H} Z`;
  const errorPoints = pointsFor(errors ?? [], 0);
  const errorLine = errors
    ? smoothPath(
        errorPoints.map((point) => ({ x: point.x, y: BOTTOM - (BOTTOM - point.y) * 0.12 - 2 })),
      )
    : "";

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      preserveAspectRatio="none"
      role="img"
      aria-label="Request volume over the last 24 hours"
      className={cn("h-[170px] w-full", className)}
    >
      {[0, 42.5, 85, 127.5, 169.5].map((y) => (
        <line
          key={y}
          x1={0}
          x2={W}
          y1={y}
          y2={y}
          stroke="var(--octra-chart-grid)"
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />
      ))}

      <path d={area} fill="var(--octra-chart-area)" />
      <path
        d={line}
        fill="none"
        stroke="var(--octra-primary)"
        strokeWidth={2}
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
      {errors ? (
        <path
          d={errorLine}
          fill="none"
          stroke="var(--octra-coral)"
          strokeWidth={1.5}
          strokeLinecap="round"
          vectorEffect="non-scaling-stroke"
        />
      ) : null}
    </svg>
  );
}
