import { cn } from "@octra/ui";

export interface FabricGateway {
  id: string;
  label: string;
  value: number;
  display: string;
  share: string;
  color: string;
}

export interface FabricDestination {
  id: string;
  label: string;
  value: number;
  display: string;
  share: string;
}

export interface FabricFlow {
  gateway: string;
  destination: string;
  value: number;
  lead?: boolean;
}

export interface FabricData {
  gateways: FabricGateway[];
  destinations: FabricDestination[];
  flows: FabricFlow[];
}

const WIDTH = 1128;
const HEIGHT = 360;
const FABRIC_LEFT = 160;
const FABRIC_RIGHT = 940;
const CONTROL_X = 555;
const SCALE = 220;
const BAND_GAP = 28;
const START_Y = 36;
const FLOW_GAP = 2;

interface Geometry {
  gatewayY: number;
  gatewayHeight: number;
  sourceTop: number;
  sourceBottom: number;
  destinationTop: number;
  destinationBottom: number;
}

function buildGeometry(data: FabricData) {
  const total = data.gateways.reduce((sum, gateway) => sum + gateway.value, 0) || 1;
  const thickness = (value: number) => (value / total) * SCALE;

  const destinationGeometry = new Map<string, { top: number; bottom: number }>();
  data.destinations.forEach((destination, index) => {
    const center = 46 + index * 43;
    const t = thickness(destination.value);
    destinationGeometry.set(destination.id, { top: center - t / 2, bottom: center + t / 2 });
  });

  const flows: Array<Geometry & FabricFlow & { color: string }> = [];

  let bandY = START_Y;
  for (const gateway of data.gateways) {
    const gatewayFlows = data.flows.filter((flow) => flow.gateway === gateway.id);
    const heights = gatewayFlows.map((flow) => thickness(flow.value));
    const bandHeight = heights.reduce((sum, h) => sum + h, 0) + FLOW_GAP * gatewayFlows.length;

    let cursor = bandY;
    for (const [index, flow] of gatewayFlows.entries()) {
      const t = heights[index] ?? 0;
      const destination = destinationGeometry.get(flow.destination);
      if (destination) {
        flows.push({
          ...flow,
          color: gateway.color,
          gatewayY: bandY,
          gatewayHeight: bandHeight,
          sourceTop: cursor,
          sourceBottom: cursor + t,
          destinationTop: destination.top,
          destinationBottom: destination.bottom,
        });
      }
      cursor += t + FLOW_GAP;
    }

    bandY += bandHeight + BAND_GAP;
  }

  return { total, flows };
}

function ribbonPath(flow: Geometry) {
  const x0 = FABRIC_LEFT;
  const x1 = FABRIC_RIGHT;
  return [
    `M${x0} ${flow.sourceTop}`,
    `C${CONTROL_X} ${flow.sourceTop} ${CONTROL_X} ${flow.destinationTop} ${x1} ${flow.destinationTop}`,
    `L${x1} ${flow.destinationBottom}`,
    `C${CONTROL_X} ${flow.destinationBottom} ${CONTROL_X} ${flow.sourceBottom} ${x0} ${flow.sourceBottom}`,
    "Z",
  ].join(" ");
}

export interface TrafficFabricProps {
  data: FabricData;
  className?: string;
}

export function TrafficFabric({ data, className }: TrafficFabricProps) {
  const { total, flows } = buildGeometry(data);

  return (
    <svg
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
      role="img"
      aria-label="Request flow from each gateway to its upstream destinations"
      className={cn("h-auto w-full", className)}
    >
      <defs>
        <linearGradient id="fade-lead" x1="0" x2="1">
          <stop offset="0%" stopColor="var(--octra-primary)" stopOpacity="0.22" />
          <stop offset="100%" stopColor="var(--octra-primary)" stopOpacity="0.08" />
        </linearGradient>
        {data.gateways.map((gateway) => (
          <linearGradient key={gateway.id} id={`fade-${gateway.id}`} x1="0" x2="1">
            <stop offset="0%" stopColor={gateway.color} stopOpacity="0.22" />
            <stop offset="100%" stopColor={gateway.color} stopOpacity="0.08" />
          </linearGradient>
        ))}
      </defs>

      {flows
        .filter((flow) => !flow.lead)
        .map((flow) => (
          <path
            key={`${flow.gateway}-${flow.destination}`}
            d={ribbonPath(flow)}
            fill="var(--octra-neutral-flow)"
          />
        ))}

      {flows
        .filter((flow) => flow.lead)
        .map((flow) => (
          <g key={`${flow.gateway}-${flow.destination}`}>
            <path d={ribbonPath(flow)} fill="url(#fade-lead)" />
            <path
              d={`M${FABRIC_LEFT} ${flow.sourceTop} C${CONTROL_X} ${flow.sourceTop} ${CONTROL_X} ${flow.destinationTop} ${FABRIC_RIGHT} ${flow.destinationTop}`}
              fill="none"
              stroke="var(--octra-primary)"
              strokeWidth="2"
              vectorEffect="non-scaling-stroke"
            />
          </g>
        ))}

      {data.gateways.map((gateway) => {
        const geo = flows.find((flow) => flow.gateway === gateway.id);
        if (!geo) {
          return null;
        }
        return (
          <g key={gateway.id}>
            <rect
              x={152}
              y={geo.gatewayY}
              width={8}
              height={geo.gatewayHeight}
              rx={4}
              fill={gateway.color}
            />
            <text
              x={140}
              y={geo.gatewayY + geo.gatewayHeight / 2 - 12}
              textAnchor="end"
              style={{ fontFamily: "var(--font-sans)", fontSize: 11.5, fill: "var(--octra-muted)" }}
            >
              {gateway.label}
            </text>
            <text
              x={140}
              y={geo.gatewayY + geo.gatewayHeight / 2 + 8}
              textAnchor="end"
              style={{
                fontFamily: "var(--font-mono)",
                fontSize: 16,
                fontWeight: 500,
                fill: "var(--octra-foreground)",
              }}
            >
              {gateway.display}
            </text>
            <text
              x={140}
              y={geo.gatewayY + geo.gatewayHeight / 2 + 26}
              textAnchor="end"
              style={{ fontFamily: "var(--font-sans)", fontSize: 10.5, fill: "var(--octra-muted)" }}
            >
              {gateway.share} of traffic
            </text>
          </g>
        );
      })}

      {data.destinations.map((destination, index) => {
        const t = (destination.value / total) * SCALE;
        const center = 46 + index * 43;
        return (
          <g key={destination.id}>
            <text
              x={960}
              y={center - t / 2 + 10}
              style={{
                fontFamily: "var(--font-sans)",
                fontSize: 11.5,
                fill: "var(--octra-secondary)",
              }}
            >
              {destination.label}
            </text>
            <text
              x={960}
              y={center - t / 2 + 25}
              style={{ fontFamily: "var(--font-mono)", fontSize: 10.5, fill: "var(--octra-muted)" }}
            >
              {destination.display} · {destination.share}
            </text>
          </g>
        );
      })}
    </svg>
  );
}
