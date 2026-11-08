import { Navigate, Route, Routes } from "react-router-dom";

import { AppShell } from "../components/app-shell";
import { AgentsPage } from "./pages/agents";
import { AiControlPage } from "./pages/ai-control";
import { Dashboard } from "./pages/dashboard";
import { GatewayPage } from "./pages/gateway";
import { LlmPage } from "./pages/llm";
import { McpPage } from "./pages/mcp";
import { PluginsPage } from "./pages/plugins";
import { SettingsPage } from "./pages/settings";

export function App() {
  return (
    <Routes>
      <Route element={<AppShell />}>
        <Route index element={<Dashboard />} />
        <Route path="gateway" element={<GatewayPage />} />
        <Route path="llm" element={<LlmPage />} />
        <Route path="agents" element={<AgentsPage />} />
        <Route path="ai-control" element={<AiControlPage />} />
        <Route path="mcp" element={<McpPage />} />
        <Route path="plugins" element={<PluginsPage />} />
        <Route path="settings" element={<SettingsPage />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}
