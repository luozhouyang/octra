import { PlaceholderPage } from "../../components/placeholder-page";

export function PluginsPage() {
  return (
    <PlaceholderPage
      title="Plugins"
      phase="Phase 4 & 9"
      description="Rust-native extensions compiled into the daemon, with a WASM sandbox for third-party code later."
      capabilities={[
        "Plugin metadata, registry, lifecycle, and configuration",
        "Scoped execution contexts for API, LLM, and MCP stages",
        "Built-in plugins and a permission model",
        "WASM runtime with filesystem, network, CPU, and memory limits",
      ]}
    />
  );
}
