import { useState } from "react";
import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { cn } from "@/lib/utils";
import {
  Activity,
  Server,
  HardDrive,
  LayoutGrid,
  TerminalSquare,
  LogOut,
  Boxes,
  Menu,
  X,
  Cloud,
  Gauge,
  Globe,
  ExternalLink,
} from "lucide-react";

const NAV_ITEMS = [
  { to: "/overview", icon: Activity, label: "Overview" },
  { to: "/nodes", icon: Server, label: "Nodes" },
  { to: "/disks", icon: HardDrive, label: "Disks" },
  { to: "/backups", icon: Cloud, label: "Backups" },
  { to: "/apps", icon: LayoutGrid, label: "Apps" },
  { to: "/terminal", icon: TerminalSquare, label: "Terminal" },
];

function SidebarContent({
  onClose,
  onLogout,
}: {
  onClose?: () => void;
  onLogout: () => void;
}) {
  return (
    <div className="flex h-full flex-col">
      {/* Logo */}
      <div className="flex items-center gap-2.5 px-5 py-5 border-b border-[#27272a]">
        <Boxes className="h-5 w-5 text-[#a78bfa]" strokeWidth={1.5} />
        <span className="text-sm font-semibold text-[#fafafa] tracking-tight">
          YoLab
        </span>
        {onClose && (
          <button
            onClick={onClose}
            className="ml-auto text-[#71717a] hover:text-[#fafafa] transition-colors"
          >
            <X className="h-4 w-4" />
          </button>
        )}
      </div>

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
                  ? "bg-[#a78bfa]/10 text-[#a78bfa] font-medium"
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

export function Layout({ onLogout }: { onLogout: () => void }) {
  const [mobileOpen, setMobileOpen] = useState(false);
  const navigate = useNavigate();

  async function handleLogout() {
    await fetch("/api/logout", { method: "POST" });
    onLogout();
    navigate("/");
  }

  return (
    <div className="flex h-full bg-[#09090b]">
      {/* Desktop sidebar */}
      <aside className="hidden md:flex md:flex-col md:w-56 md:flex-shrink-0 border-r border-[#27272a] bg-[#111114]">
        <SidebarContent onLogout={handleLogout} />
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
          "fixed inset-y-0 left-0 z-50 w-64 bg-[#111114] border-r border-[#27272a] transform transition-transform duration-200 md:hidden",
          mobileOpen ? "translate-x-0" : "-translate-x-full",
        )}
      >
        <SidebarContent
          onClose={() => setMobileOpen(false)}
          onLogout={handleLogout}
        />
      </aside>

      {/* Main content */}
      <div className="flex flex-1 flex-col min-w-0 overflow-hidden">
        {/* Mobile top bar */}
        <header className="flex md:hidden items-center gap-3 px-4 py-3 border-b border-[#27272a] bg-[#111114]">
          <button
            onClick={() => setMobileOpen(true)}
            className="text-[#71717a] hover:text-[#fafafa] transition-colors"
          >
            <Menu className="h-5 w-5" />
          </button>
          <Boxes className="h-4 w-4 text-[#a78bfa]" strokeWidth={1.5} />
          <span className="text-sm font-semibold text-[#fafafa]">YoLab</span>
        </header>

        <main className="flex-1 overflow-y-auto px-4 py-6 md:px-8 md:py-8">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
