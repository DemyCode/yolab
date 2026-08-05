import { NavLink, Outlet } from "react-router-dom";
import { Home, Plus, Settings2, Moon, Sun, LogOut } from "lucide-react";
import { cn } from "@/lib/utils";
import { useTheme } from "@/lib/theme";
import { api } from "@/lib/api";
import { Wordmark } from "@/components/Logo";

/**
 * Three destinations.
 *
 * The previous shell had six, five of which were infrastructure: Overview,
 * Machines, Storage, Backups, Apps, Terminal. That is a diagram of the system's
 * architecture offered as a menu — someone who set this box up to get their
 * photos off Google opened it and found a storage administration console with
 * an app store hidden inside.
 *
 * Now: the things you own, somewhere to get more, and everything else. Storage,
 * machines, backups and the terminal still exist, one level down under Settings,
 * where they read as settings rather than as the point of the product.
 */
const NAV = [
  { to: "/", icon: Home, label: "Home", end: true },
  { to: "/add", icon: Plus, label: "Add" },
  { to: "/box", icon: Settings2, label: "Settings" },
];

function ThemeButton({ compact }: { compact?: boolean }) {
  const { resolved, setTheme } = useTheme();
  const next = resolved === "dark" ? "light" : "dark";
  const Icon = resolved === "dark" ? Sun : Moon;
  return (
    <button
      onClick={() => setTheme(next)}
      className={cn(
        "flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm text-fg-muted transition-colors hover:bg-surface-2 hover:text-fg",
        compact ? "" : "w-full",
      )}
      aria-label={`Switch to ${next} theme`}
    >
      <Icon className="h-[18px] w-[18px] shrink-0" strokeWidth={1.75} />
      {!compact && <span>{resolved === "dark" ? "Light" : "Dark"} theme</span>}
    </button>
  );
}

export function AppShell({ onLogout }: { onLogout: () => void }) {
  async function signOut() {
    try {
      await api.post("/api/logout");
    } catch {
      // Even if the call fails the local session should end — leaving someone
      // apparently signed in after they asked to leave is the worse outcome.
    }
    onLogout();
  }

  return (
    <div className="flex min-h-full bg-bg">
      {/* Desktop rail */}
      <aside className="sticky top-0 hidden h-screen w-60 shrink-0 flex-col border-r border-border bg-surface px-3 py-5 md:flex">
        <div className="px-3 pb-5">
          <Wordmark />
        </div>

        <nav className="flex-1 space-y-1">
          {NAV.map(({ to, icon: Icon, label, end }) => (
            <NavLink
              key={to}
              to={to}
              end={end}
              className={({ isActive }) =>
                cn(
                  "flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm transition-colors",
                  isActive
                    ? "bg-primary-soft font-medium text-primary"
                    : "text-fg-muted hover:bg-surface-2 hover:text-fg",
                )
              }
            >
              <Icon className="h-[18px] w-[18px] shrink-0" strokeWidth={1.75} />
              {label}
            </NavLink>
          ))}
        </nav>

        <div className="space-y-1 border-t border-border pt-3">
          <ThemeButton />
          <button
            onClick={() => void signOut()}
            className="flex w-full items-center gap-3 rounded-xl px-3 py-2.5 text-sm text-fg-muted transition-colors hover:bg-surface-2 hover:text-fg"
          >
            <LogOut className="h-[18px] w-[18px] shrink-0" strokeWidth={1.75} />
            Sign out
          </button>
        </div>
      </aside>

      {/* Content. The bottom padding on mobile clears the tab bar, including
          the iOS home indicator. */}
      <main className="min-w-0 flex-1 pb-[calc(4.5rem+env(safe-area-inset-bottom))] md:pb-0">
        <Outlet />
      </main>

      {/* Mobile tab bar */}
      <nav className="fixed inset-x-0 bottom-0 z-40 flex border-t border-border bg-surface/95 pb-[env(safe-area-inset-bottom)] backdrop-blur md:hidden">
        {NAV.map(({ to, icon: Icon, label, end }) => (
          <NavLink
            key={to}
            to={to}
            end={end}
            className={({ isActive }) =>
              cn(
                "flex flex-1 flex-col items-center gap-1 py-2.5 text-[11px] font-medium transition-colors",
                isActive ? "text-primary" : "text-fg-subtle",
              )
            }
          >
            <Icon className="h-6 w-6" strokeWidth={1.75} />
            {label}
          </NavLink>
        ))}
      </nav>
    </div>
  );
}

/** Page frame: one max width, one set of gutters, one title treatment. */
export function Page({
  title,
  subtitle,
  action,
  children,
  wide,
}: {
  title?: string;
  subtitle?: string;
  action?: React.ReactNode;
  children: React.ReactNode;
  wide?: boolean;
}) {
  return (
    <div
      className={cn(
        "mx-auto w-full px-5 py-6 md:px-8 md:py-8",
        wide ? "max-w-6xl" : "max-w-3xl",
      )}
    >
      {(title || action) && (
        <header className="mb-6 flex items-start gap-4">
          <div className="min-w-0 flex-1">
            {title && (
              <h1 className="font-display text-[1.75rem] leading-tight text-fg md:text-4xl">
                {title}
              </h1>
            )}
            {subtitle && (
              <p className="mt-1.5 text-sm text-fg-muted">{subtitle}</p>
            )}
          </div>
          {action}
        </header>
      )}
      {children}
    </div>
  );
}
