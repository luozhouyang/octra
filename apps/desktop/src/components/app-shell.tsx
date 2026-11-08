import { useQuery } from "@tanstack/react-query";
import { cn } from "@octra/ui";
import {
  Bot,
  BrainCircuit,
  CircleHelp,
  Hexagon,
  LayoutDashboard,
  Menu,
  Moon,
  Plug,
  Puzzle,
  RefreshCw,
  Route,
  Search,
  Settings,
  Sparkles,
  Sun,
  X,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { NavLink, Outlet, useLocation, useNavigate } from "react-router-dom";

import { formatUptime, hostFromBaseUrl } from "../lib/format";
import { fetchHealth } from "../lib/control-api";
import { useControlSettings } from "../lib/store";
import { useTheme } from "../lib/theme";

interface NavItem {
  to: string;
  label: string;
  section: "Operate" | "Control" | "System";
  icon: LucideIcon;
  signal: string;
  end?: boolean;
}

const navigation: NavItem[] = [
  {
    to: "/",
    label: "Overview",
    section: "Operate",
    icon: LayoutDashboard,
    signal: "bg-primary",
    end: true,
  },
  { to: "/gateway", label: "API Gateway", section: "Operate", icon: Route, signal: "bg-sky" },
  { to: "/llm", label: "LLM Gateway", section: "Operate", icon: Sparkles, signal: "bg-violet" },
  { to: "/mcp", label: "MCP Gateway", section: "Operate", icon: Plug, signal: "bg-amber" },
  { to: "/agents", label: "Agents", section: "Control", icon: Bot, signal: "bg-coral" },
  {
    to: "/ai-control",
    label: "AI Control",
    section: "Control",
    icon: BrainCircuit,
    signal: "bg-violet",
  },
  {
    to: "/plugins",
    label: "Plugins",
    section: "Control",
    icon: Puzzle,
    signal: "bg-muted-foreground",
  },
];

const settingsNav: NavItem = {
  to: "/settings",
  label: "Settings",
  section: "System",
  icon: Settings,
  signal: "bg-muted-foreground",
};

function useHealth() {
  return useQuery({
    queryKey: ["health"],
    queryFn: fetchHealth,
    refetchInterval: 5_000,
    retry: 1,
  });
}

export function AppShell() {
  const location = useLocation();
  const navigate = useNavigate();
  const health = useHealth();
  const theme = useTheme((state) => state.theme);
  const toggleTheme = useTheme((state) => state.toggleTheme);
  const baseUrl = useControlSettings((state) => state.baseUrl);
  const [navOpen, setNavOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);

  const current = useMemo(
    () =>
      location.pathname.startsWith("/settings")
        ? settingsNav
        : (navigation.find((item) =>
            item.end ? location.pathname === item.to : location.pathname.startsWith(item.to),
          ) ?? navigation[0]!),
    [location.pathname],
  );

  useEffect(() => {
    setNavOpen(false);
  }, [location.pathname]);

  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((open) => !open);
      }
      if (event.key === "Escape") {
        setPaletteOpen(false);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const status = health.isPending
    ? "Connecting"
    : health.isError
      ? "Offline"
      : health.data?.status === "ok"
        ? "Online"
        : "Degraded";
  const statusColor = health.isError
    ? "bg-danger"
    : health.data?.status === "ok"
      ? "bg-success"
      : "bg-warning";
  const version = health.data?.version ?? "—";
  const uptime = health.data ? formatUptime(health.data.uptime_secs) : "—";
  const storage = health.data?.storage === "ok" ? "SQLite OK" : "Storage —";

  return (
    <div className="flex h-screen overflow-hidden bg-background text-foreground">
      {navOpen ? (
        <button
          type="button"
          aria-label="Close navigation"
          className="fixed inset-0 z-30 bg-black/50 backdrop-blur-sm lg:hidden"
          onClick={() => setNavOpen(false)}
        />
      ) : null}

      <aside
        className={cn(
          "fixed inset-y-0 left-0 z-40 flex w-[248px] shrink-0 flex-col border-r border-border bg-surface p-4 transition-transform duration-200 lg:static lg:translate-x-0",
          navOpen ? "translate-x-0" : "-translate-x-full",
        )}
      >
        <div className="flex items-center justify-between">
          <NavLink to="/" className="flex items-center gap-2.5">
            <span className="flex h-8 w-8 items-center justify-center rounded-[10px] border border-primary-line bg-primary-soft">
              <Hexagon className="h-[18px] w-[18px] text-primary" strokeWidth={1.5} />
            </span>
            <span className="flex flex-col">
              <span className="text-base font-medium leading-tight">Octra</span>
              <span className="font-mono text-[10.5px] leading-tight text-muted-foreground">
                Local gateway
              </span>
            </span>
          </NavLink>
          <button
            type="button"
            aria-label="Close navigation"
            className="text-muted-foreground hover:text-foreground lg:hidden"
            onClick={() => setNavOpen(false)}
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        <div className="h-6" />
        <SidebarSection
          label="Operate"
          items={navigation.filter((item) => item.section === "Operate")}
        />
        <div className="h-6" />
        <SidebarSection
          label="Control"
          items={navigation.filter((item) => item.section === "Control")}
        />

        <div className="flex-1" />

        <div className="rounded-xl border border-border bg-card p-3">
          <div className="flex items-center gap-2">
            <span className={cn("h-1.5 w-1.5 rounded-full", statusColor)} />
            <span className="text-xs font-medium text-secondary">Daemon {status}</span>
            <span className="ml-auto font-mono text-[10.5px] text-muted-foreground">
              v{version}
            </span>
          </div>
          <div className="mt-2.5 flex items-center justify-between font-mono text-[10.5px] text-muted-foreground">
            <span>{uptime} uptime</span>
            <span>{storage}</span>
          </div>
        </div>

        <div className="h-2" />
        <NavLink
          to="/settings"
          className={({ isActive }) =>
            cn(
              "flex h-9 items-center gap-2.5 rounded-[10px] px-3 text-[13px] transition-colors",
              isActive
                ? "bg-primary-soft text-foreground"
                : "text-secondary hover:bg-raised hover:text-foreground",
            )
          }
        >
          <Settings className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
          Settings
          <span className="ml-auto font-mono text-[10.5px] text-muted-foreground">⌘,</span>
        </NavLink>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-[60px] shrink-0 items-center gap-4 border-b border-border px-4 sm:px-6">
          <button
            type="button"
            aria-label="Open navigation"
            className="text-muted-foreground hover:text-foreground lg:hidden"
            onClick={() => setNavOpen(true)}
          >
            <Menu className="h-[18px] w-[18px]" />
          </button>

          <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-2 text-xs">
            <span className="hidden text-muted-foreground sm:inline">{current.section}</span>
            <span className="hidden text-muted-foreground sm:inline">/</span>
            <span className="truncate text-secondary">{current.label}</span>
          </nav>

          <div className="ml-auto flex items-center gap-2">
            <button
              type="button"
              onClick={() => setPaletteOpen(true)}
              className="hidden h-8 w-[300px] items-center gap-2 rounded-lg border border-border bg-surface px-2.5 text-left text-xs text-muted-foreground transition-colors hover:border-border-strong md:flex"
            >
              <Search className="h-3.5 w-3.5" />
              Search
              <span className="ml-auto font-mono text-[10px]">⌘K</span>
            </button>

            <span className="hidden items-center gap-2 rounded-lg border border-border bg-surface px-2.5 py-1.5 xl:flex">
              <span className={cn("h-1.5 w-1.5 rounded-full", statusColor)} />
              <span className="font-mono text-[11px] text-secondary">
                {hostFromBaseUrl(baseUrl)}
              </span>
            </span>

            <button
              type="button"
              aria-label="Reload snapshot"
              onClick={() => void health.refetch()}
              className="flex h-8 w-8 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-raised hover:text-foreground"
            >
              <RefreshCw className={cn("h-4 w-4", health.isFetching && "animate-spin")} />
            </button>

            <button
              type="button"
              aria-label={theme === "dark" ? "Switch to light mode" : "Switch to dark mode"}
              onClick={toggleTheme}
              className="flex h-8 w-8 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-raised hover:text-foreground"
            >
              {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
            </button>

            <button
              type="button"
              aria-label="Help"
              className="hidden h-8 w-8 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-raised hover:text-foreground sm:flex"
            >
              <CircleHelp className="h-4 w-4" />
            </button>
          </div>
        </header>

        <main className="flex-1 overflow-y-auto">
          <div className="mx-auto w-full max-w-[1600px] px-4 py-6 sm:px-6 lg:px-8 lg:py-8">
            <Outlet />
          </div>
        </main>
      </div>

      {paletteOpen ? (
        <CommandPalette
          onClose={() => setPaletteOpen(false)}
          onNavigate={(to) => {
            setPaletteOpen(false);
            navigate(to);
          }}
        />
      ) : null}
    </div>
  );
}

function SidebarSection({ label, items }: { label: string; items: NavItem[] }) {
  return (
    <div>
      <p className="px-3 font-mono text-[10.5px] text-muted-foreground">{label}</p>
      <nav className="mt-2 flex flex-col gap-0.5">
        {items.map((item) => (
          <NavLink
            key={item.to}
            to={item.to}
            end={item.end}
            className={({ isActive }) =>
              cn(
                "group flex h-9 items-center gap-2.5 rounded-[10px] px-3 text-[13px] transition-colors",
                isActive
                  ? "bg-primary-soft text-foreground shadow-[inset_0_0_0_1px_var(--octra-border)]"
                  : "text-secondary hover:bg-raised hover:text-foreground",
              )
            }
          >
            {({ isActive }) => (
              <>
                <item.icon
                  className={cn("h-4 w-4", isActive ? "text-primary" : "text-muted-foreground")}
                  strokeWidth={1.5}
                />
                <span className="truncate">{item.label}</span>
                <span className={cn("ml-auto h-1.5 w-1.5 rounded-full", item.signal)} />
              </>
            )}
          </NavLink>
        ))}
      </nav>
    </div>
  );
}

function CommandPalette({
  onClose,
  onNavigate,
}: {
  onClose: () => void;
  onNavigate: (to: string) => void;
}) {
  const [query, setQuery] = useState("");
  const results = [...navigation, settingsNav].filter((item) =>
    item.label.toLowerCase().includes(query.trim().toLowerCase()),
  );

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/45 px-4 pt-[18vh] backdrop-blur-[2px]"
      onClick={onClose}
    >
      <div
        className="w-full max-w-md overflow-hidden rounded-2xl border border-border-strong bg-card shadow-2xl"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-center gap-2.5 border-b border-border px-4 py-3">
          <Search className="h-4 w-4 text-muted-foreground" />
          <input
            autoFocus
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && results[0]) {
                onNavigate(results[0].to);
              }
            }}
            placeholder="Go to"
            className="w-full bg-transparent text-sm outline-none placeholder:text-muted-foreground"
          />
          <span className="font-mono text-[10px] text-muted-foreground">esc</span>
        </div>
        <ul className="max-h-72 overflow-y-auto p-1.5">
          {results.length === 0 ? (
            <li className="px-3 py-6 text-center text-xs text-muted-foreground">No matches</li>
          ) : (
            results.map((item) => (
              <li key={item.to}>
                <button
                  type="button"
                  onClick={() => onNavigate(item.to)}
                  className="flex w-full items-center gap-2.5 rounded-lg px-3 py-2 text-left text-[13px] text-secondary hover:bg-raised hover:text-foreground"
                >
                  <item.icon className="h-4 w-4 text-muted-foreground" strokeWidth={1.5} />
                  {item.label}
                  <span className="ml-auto font-mono text-[10px] text-muted-foreground">
                    {item.section}
                  </span>
                </button>
              </li>
            ))
          )}
        </ul>
      </div>
    </div>
  );
}
