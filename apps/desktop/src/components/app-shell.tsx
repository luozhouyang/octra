import { cn } from "@octra/ui";
import { NavLink, Outlet } from "react-router-dom";

interface NavigationItem {
  to: string;
  label: string;
  badge: string;
  end?: boolean;
}

const navigation: NavigationItem[] = [
  { to: "/", label: "Dashboard", badge: "DB", end: true },
  { to: "/gateway", label: "API Gateway", badge: "API" },
  { to: "/llm", label: "LLM Gateway", badge: "LLM" },
  { to: "/agents", label: "Agents", badge: "AG" },
  { to: "/mcp", label: "MCP", badge: "MCP" },
  { to: "/plugins", label: "Plugins", badge: "PL" },
  { to: "/settings", label: "Settings", badge: "ST" },
];

export function AppShell() {
  return (
    <div className="flex h-screen overflow-hidden bg-background text-foreground">
      <aside className="flex w-64 shrink-0 flex-col border-r border-border bg-card">
        <div className="flex items-center gap-3 px-6 py-6">
          <div className="flex h-9 w-9 items-center justify-center rounded-xl bg-primary text-sm font-bold text-primary-foreground">
            O
          </div>
          <div>
            <p className="text-sm font-semibold tracking-wide">Octra</p>
            <p className="text-xs text-muted-foreground">Local Gateway</p>
          </div>
        </div>

        <nav className="flex flex-1 flex-col gap-1 px-3">
          {navigation.map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              end={item.end}
              className={({ isActive }) =>
                cn(
                  "flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors",
                  isActive
                    ? "bg-muted text-foreground"
                    : "text-muted-foreground hover:bg-muted/60 hover:text-foreground",
                )
              }
            >
              <span className="flex h-6 w-9 items-center justify-center rounded bg-background/60 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                {item.badge}
              </span>
              {item.label}
            </NavLink>
          ))}
        </nav>

        <div className="border-t border-border px-6 py-4 text-xs text-muted-foreground">
          v0.1.0 · local-first
        </div>
      </aside>

      <main className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-5xl px-8 py-10">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
