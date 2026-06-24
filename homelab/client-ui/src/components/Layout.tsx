import { useState, useEffect, useCallback } from "react";
import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { cn } from "@/lib/utils";
import {
  Activity,
  Server,
  LayoutGrid,
  TerminalSquare,
  LogOut,
  Boxes,
  Menu,
  X,
  Cloud,
  Gauge,
  Globe,
  Database,
  ExternalLink,
  AlertTriangle,
  AlertOctagon,
  CheckCircle2,
  ChevronDown,
} from "lucide-react";

type HealthLevel = "ok" | "warn" | "error";
interface HealthIssue {
  level: HealthLevel;
  title: string;
  description: string;
}
interface ClusterHealth {
  level: HealthLevel;
  title: string;
  message: string;
  issues: HealthIssue[];
}

const NAV_ITEMS = [
  { to: "/overview", icon: Activity, label: "Overview" },
  { to: "/nodes", icon: Server, label: "Machines" },
  { to: "/storage", icon: Database, label: "Storage" },
  { to: "/backups", icon: Cloud, label: "Backups" },
  { to: "/apps", icon: LayoutGrid, label: "Apps" },
  { to: "/terminal", icon: TerminalSquare, label: "Terminal" },
];

function SidebarContent({
  onClose,
  onLogout,
  health,
}: {
  onClose?: () => void;
  onLogout: () => void;
  health: ClusterHealth | null;
}) {
  const isError = health?.level === "error";
  const isWarn = health?.level === "warn";

  return (
    <div className="flex h-full flex-col">
      {/* Logo */}
      <div className={cn(
        "flex items-center gap-2.5 px-5 py-5 border-b",
        isError ? "border-[#f87171]/20" : "border-[#27272a]",
      )}>
        <Boxes className={cn("h-5 w-5", isError ? "text-[#f87171]" : "text-[#a78bfa]")} strokeWidth={1.5} />
        <span className="text-sm font-semibold text-[#fafafa] tracking-tight">
          YoLab
        </span>
        {isError && (
          <span className="ml-auto inline-block w-2 h-2 rounded-full bg-[#f87171] animate-pulse" />
        )}
        {isWarn && (
          <span className="ml-auto inline-block w-2 h-2 rounded-full bg-[#fbbf24]" />
        )}
        {onClose && (
          <button
            onClick={onClose}
            className={cn("text-[#71717a] hover:text-[#fafafa] transition-colors", (isError || isWarn) ? "" : "ml-auto")}
          >
            <X className="h-4 w-4" />
          </button>
        )}
      </div>

      {/* Health summary in sidebar for non-OK states */}
      {health && health.level !== "ok" && (
        <div className={cn(
          "mx-3 mt-3 rounded-md px-3 py-2 text-xs",
          isError ? "bg-[#f87171]/10 text-[#f87171]" : "bg-[#fbbf24]/10 text-[#fbbf24]",
        )}>
          {isError ? (
            <div className="flex items-center gap-1.5">
              <AlertOctagon className="h-3.5 w-3.5 flex-shrink-0" />
              <span className="font-medium">Storage error</span>
            </div>
          ) : (
            <div className="flex items-center gap-1.5">
              <AlertTriangle className="h-3.5 w-3.5 flex-shrink-0" />
              <span className="font-medium">Storage warning</span>
            </div>
          )}
        </div>
      )}

      {/* Health OK indicator */}
      {health?.level === "ok" && (
        <div className="mx-3 mt-3 rounded-md px-3 py-2 text-xs bg-[#4ade80]/10 text-[#4ade80] flex items-center gap-1.5">
          <CheckCircle2 className="h-3.5 w-3.5 flex-shrink-0" />
          <span className="font-medium">Storage healthy</span>
        </div>
      )}

      {/* Nav */}
      <nav className="flex-1 px-3 py-4 space-y-0.5">
        {NAV_ITEMS.map(({ to, icon: Icon, label }) => (
          <NavLink
            key={to}
            to={to}
            onClick={onClose}
            className={({ isActive }) =>
              cn(
                "flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors",
                isActive
                  ? isError
                    ? "bg-[#f87171]/10 text-[#f87171] font-medium"
                    : "bg-[#a78bfa]/10 text-[#a78bfa] font-medium"
                  : "text-[#a1a1aa] hover:bg-[#18181b] hover:text-[#fafafa]",
              )
            }
          >
            <Icon className="h-4 w-4 flex-shrink-0" strokeWidth={1.75} />
            {label}
          </NavLink>
        ))}

      </nav>

      {/* Bottom */}
      <div className="px-3 py-4 border-t border-[#27272a] space-y-0.5">
        <button
          onClick={async () => {
            try {
              const res = await fetch("/api/account/token");
              const { account_token } = await res.json();
              window.open(
                `https://demycode.ovh/console#token=${encodeURIComponent(account_token)}`,
                "_blank"
              );
            } catch {
              window.open("https://demycode.ovh/console", "_blank");
            }
          }}
          className="flex w-full items-center gap-3 rounded-md px-3 py-2 text-sm text-[#a1a1aa] hover:bg-[#18181b] hover:text-[#fafafa] transition-colors"
        >
          <Globe className="h-4 w-4 flex-shrink-0" strokeWidth={1.75} />
          Account dashboard
          <ExternalLink className="h-3 w-3 ml-auto opacity-50" />
        </button>
        <a
          href="/glances/"
          target="_blank"
          rel="noopener noreferrer"
          className="flex w-full items-center gap-3 rounded-md px-3 py-2 text-sm text-[#a1a1aa] hover:bg-[#18181b] hover:text-[#fafafa] transition-colors"
        >
          <Gauge className="h-4 w-4 flex-shrink-0" strokeWidth={1.75} />
          System monitor
          <ExternalLink className="h-3 w-3 ml-auto opacity-50" />
        </a>
        <button
          onClick={onLogout}
          className="flex w-full items-center gap-3 rounded-md px-3 py-2 text-sm text-[#71717a] hover:bg-[#18181b] hover:text-[#fafafa] transition-colors"
        >
          <LogOut className="h-4 w-4 flex-shrink-0" strokeWidth={1.75} />
          Sign out
        </button>
      </div>
    </div>
  );
}

function ClusterHealthBanner({ health }: { health: ClusterHealth | null }) {
  const [expanded, setExpanded] = useState(false);

  if (!health || health.level === "ok") return null;

  const isError = health.level === "error";
  const colors = isError
    ? {
        bg: "bg-[#f87171]/10 border-[#f87171]/40",
        icon: "text-[#f87171]",
        title: "text-[#f87171]",
        msg: "text-[#fca5a5]",
        badge: "bg-[#f87171]/20 text-[#f87171]",
        chevron: "text-[#f87171]/70",
      }
    : {
        bg: "bg-[#fbbf24]/10 border-[#fbbf24]/40",
        icon: "text-[#fbbf24]",
        title: "text-[#fbbf24]",
        msg: "text-[#fde68a]",
        badge: "bg-[#fbbf24]/20 text-[#fbbf24]",
        chevron: "text-[#fbbf24]/70",
      };

  return (
    <div className={cn("border-b px-4 py-3", colors.bg)}>
      <button
        className="w-full text-left"
        onClick={() => setExpanded((e) => !e)}
      >
        <div className="flex items-center gap-2.5">
          {isError ? (
            <AlertOctagon className={cn("h-4 w-4 flex-shrink-0", colors.icon)} />
          ) : (
            <AlertTriangle className={cn("h-4 w-4 flex-shrink-0", colors.icon)} />
          )}
          <span className={cn("text-sm font-semibold flex-1", colors.title)}>
            {health.title}
          </span>
          {health.issues.length > 0 && (
            <ChevronDown
              className={cn(
                "h-3.5 w-3.5 flex-shrink-0 transition-transform",
                colors.chevron,
                expanded && "rotate-180",
              )}
            />
          )}
        </div>
        <p className={cn("text-xs mt-0.5 ml-6.5", colors.msg)}>{health.message}</p>
      </button>
      {expanded && health.issues.length > 0 && (
        <div className="mt-2 ml-6 space-y-2">
          {health.issues.map((issue, i) => (
            <div key={i} className="text-xs">
              <span className={cn(
                "font-medium",
                issue.level === "error" ? "text-[#f87171]" : "text-[#fbbf24]",
              )}>
                {issue.title}:
              </span>{" "}
              <span className="text-[#a1a1aa]">{issue.description}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export function Layout({ onLogout }: { onLogout: () => void }) {
  const [mobileOpen, setMobileOpen] = useState(false);
  const [health, setHealth] = useState<ClusterHealth | null>(null);
  const navigate = useNavigate();

  const pollHealth = useCallback(() => {
    fetch("/api/cluster/health")
      .then((r) => r.json())
      .then((d: ClusterHealth) => setHealth(d))
      .catch(() => {});
  }, []);

  useEffect(() => {
    pollHealth();
    const id = setInterval(pollHealth, 30_000);
    return () => clearInterval(id);
  }, [pollHealth]);

  async function handleLogout() {
    await fetch("/api/logout", { method: "POST" });
    onLogout();
    navigate("/");
  }

  const isError = health?.level === "error";

  return (
    <div className={cn("flex h-full", isError ? "bg-[#0f0909]" : "bg-[#09090b]")}>
      {/* Desktop sidebar */}
      <aside className={cn(
        "hidden md:flex md:flex-col md:w-56 md:flex-shrink-0 border-r",
        isError ? "border-[#f87171]/20 bg-[#110d0d]" : "border-[#27272a] bg-[#111114]",
      )}>
        <SidebarContent onLogout={handleLogout} health={health} />
      </aside>

      {/* Mobile overlay */}
      {mobileOpen && (
        <div
          className="fixed inset-0 z-40 bg-black/60 md:hidden"
          onClick={() => setMobileOpen(false)}
        />
      )}

      {/* Mobile drawer */}
      <aside
        className={cn(
          "fixed inset-y-0 left-0 z-50 w-64 border-r transform transition-transform duration-200 md:hidden",
          isError ? "bg-[#110d0d] border-[#f87171]/20" : "bg-[#111114] border-[#27272a]",
          mobileOpen ? "translate-x-0" : "-translate-x-full",
        )}
      >
        <SidebarContent
          onClose={() => setMobileOpen(false)}
          onLogout={handleLogout}
          health={health}
        />
      </aside>

      {/* Main content */}
      <div className="flex flex-1 flex-col min-w-0 overflow-hidden">
        {/* Mobile top bar */}
        <header className={cn(
          "flex md:hidden items-center gap-3 px-4 py-3 border-b",
          isError ? "border-[#f87171]/20 bg-[#110d0d]" : "border-[#27272a] bg-[#111114]",
        )}>
          <button
            onClick={() => setMobileOpen(true)}
            className="text-[#71717a] hover:text-[#fafafa] transition-colors"
          >
            <Menu className="h-5 w-5" />
          </button>
          <Boxes className="h-4 w-4 text-[#a78bfa]" strokeWidth={1.5} />
          <span className="text-sm font-semibold text-[#fafafa]">YoLab</span>
        </header>

        {/* Health banner — shown for WARN and ERROR */}
        <ClusterHealthBanner health={health} />

        {/* ERROR overlay: extra prominance strip */}
        {isError && (
          <div className="bg-[#f87171]/5 border-b border-[#f87171]/20 px-4 py-1.5 flex items-center gap-2">
            <span className="inline-block w-1.5 h-1.5 rounded-full bg-[#f87171] animate-pulse" />
            <span className="text-xs text-[#f87171]/70 font-mono">STORAGE ERROR — apps may be unavailable</span>
          </div>
        )}

        <main className="flex-1 overflow-y-auto px-4 py-6 md:px-8 md:py-8">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
