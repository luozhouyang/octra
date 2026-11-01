import { PlaceholderPage } from "../../components/placeholder-page";

export function AgentsPage() {
  return (
    <PlaceholderPage
      title="Agents"
      phase="Phase 7"
      description="Discover local coding agents and point them at Octra without breaking their existing configuration."
      capabilities={[
        "Agent registry, discovery, and health",
        "Adapters for Claude Code, Codex, and OpenCode",
        "Atomic config updates with backup and restore",
        "Per-agent model profiles and virtual model bindings",
      ]}
    />
  );
}
