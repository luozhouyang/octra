import { PlaceholderPage } from "../../components/placeholder-page";

export function McpPage() {
  return (
    <PlaceholderPage
      title="MCP Gateway"
      phase="Phase 8"
      description="A single registry and policy layer for MCP servers, tools, and sessions."
      capabilities={[
        "MCP server registry with stdio and Streamable HTTP transports",
        "Tool discovery and invocation",
        "Per-consumer and per-agent tool ACLs",
        "Invocation audit, timeouts, and health checks",
      ]}
    />
  );
}
