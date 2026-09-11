import { Link } from "react-router-dom";
import {
  ChevronRight,
  Cloud,
  CreditCard,
  Database,
  ExternalLink,
  Server,
  ScrollText,
  TerminalSquare,
  Wrench,
} from "lucide-react";
import { Page } from "@/components/AppShell";
import { Card } from "@/components/ui/card";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import { CacheDot } from "@/components/CacheDot";
import type { CacheMeta } from "@/lib/api";
import { formatBytes } from "@/lib/format";
import { useTheme, type ThemeChoice } from "@/lib/theme";
import { cn } from "@/lib/utils";
import type { StorageDetailResponse } from "@/types/storage";
import type { NodeInfo } from "@/types/nodes";
import type { StatusInfo } from "@/types/status";
import type { ClusterHealth } from "@/types/health";

/**
 * Everything that used to be five items in the main navigation.
 *
 * Each row answers the question a person would actually ask — "how much room
 * is left", "am I backed up" — as a sentence, and only then offers the page
 * where the machinery lives. Storage, machines and backups are still fully
 * available; they have simply stopped being the product's front door.
 */
function NavRow({
  to,
  href,
  onClick,
  icon: Icon,
  label,
  detail,
  tone,
  cache,
}: {
  to?: string;
  href?: string;
  /** For destinations whose URL has to be fetched at the moment of the click
   *  rather than rendered into the page — see the console row below. */
  onClick?: () => void;
  icon: typeof Database;
  label: string;
  detail?: string;
  tone?: "warn" | "error";
  /** Cache state of whatever produced `detail`, so the row can mark a
   *  remembered value while the real one is still being computed. */
  cache?: CacheMeta | null;
}) {
  const inner = (
    <>
      <div
        className={cn(
          "flex h-10 w-10 shrink-0 items-center justify-center rounded-xl",
          tone === "error"
            ? "bg-danger-soft text-danger"
            : tone === "warn"
              ? "bg-warning-soft text-warning"
              : "bg-surface-2 text-fg-muted",
        )}
      >
        <Icon className="h-5 w-5" strokeWidth={1.75} />
      </div>
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium text-fg">{label}</div>
        {detail && (
          <div className="mt-0.5 flex items-center gap-1.5 truncate text-sm text-fg-muted">
            <span className="truncate">{detail}</span>
            <CacheDot cache={cache ?? null} />
          </div>
        )}
      </div>
      {href || onClick ? (
        <ExternalLink className="h-4 w-4 shrink-0 text-fg-subtle" />
      ) : (
        <ChevronRight className="h-5 w-5 shrink-0 text-fg-subtle" />
      )}
    </>
  );

  const className =
    "flex items-center gap-4 px-5 py-4 transition-colors hover:bg-surface-2 border-b border-border last:border-0";

  if (onClick) {
    return (
      <button onClick={onClick} className={cn(className, "w-full text-left")}>
        {inner}
      </button>
    );
  }

  if (href) {
    return (
      <a
        href={href}
        target="_blank"
        rel="noopener noreferrer"
        className={className}
      >
        {inner}
      </a>
    );
  }
  return (
    <Link to={to ?? "#"} className={className}>
      {inner}
    </Link>
  );
}

const THEMES: { id: ThemeChoice; label: string }[] = [
  { id: "light", label: "Light" },
  { id: "dark", label: "Dark" },
  { id: "system", label: "Automatic" },
];

export function BoxPage() {
  const { choice, setTheme } = useTheme();

  const health = useApi<ClusterHealth>("health", "/api/cluster/health");
  const storage = useApi<StorageDetailResponse>("storage-detail", "/api/ceph/detail");
  const nodes = useApi<NodeInfo[]>("nodes", "/api/nodes");
  const status = useApi<StatusInfo>("status", "/api/status");
  const backups = useApi<{ configured: boolean }>("backups-s3", "/api/backups/s3");

  const detail = storage.data?.data;
  const storageDetail = detail
    ? `${formatBytes(detail.used_bytes)} used of ${formatBytes(detail.total_bytes)}`
    : health.data?.starting
      ? "Starting up…"
      : undefined;

  /**
   * Opens the console signed in.
   *
   * The window is opened BEFORE the await, then pointed at the URL once it
   * arrives. Opening it afterwards would be a popup triggered by a promise
   * rather than by the click, which every browser blocks. Falls back to the
   * plain console URL if the link cannot be built, so the row always goes
   * somewhere.
   */
  async function openConsole() {
    // NO "noopener" HERE, deliberately. `window.open` RETURNS NULL when that
    // flag is set — withholding the handle is precisely what the flag does — so
    // asking for it and then using the result was self-defeating: a blank tab
    // opened, `w` was null, and the code fell through to navigating the CURRENT
    // tab. Which also aborted every fetch this page had in flight, surfacing as
    // "NetworkError when attempting to fetch resource" from the pollers.
    //
    // The opener reference is dropped explicitly below instead, which gets the
    // same protection without giving up the handle.
    const w = window.open("about:blank", "_blank");

    const go = (url: string) => {
      if (w) {
        w.opener = null;
        w.location.replace(url);
      } else {
        // Popup blocked. Navigating here is worse — it costs the settings page
        // — but it is better than a click that does nothing at all.
        window.location.assign(url);
      }
    };

    try {
      const { url } = await api.get<{ url: string }>("/api/console/link");
      go(url);
    } catch {
      // Without the token the console asks for a login, which still beats a
      // dead button. If there is nothing at all to open, close the tab rather
      // than leaving a blank one behind.
      const fallback = status.data?.console_url;
      if (fallback) go(fallback);
      else w?.close();
    }
  }

  const nodeCount = nodes.data?.length ?? 0;
  const nodesDetail =
    nodeCount === 0
      ? undefined
      : nodeCount === 1
        ? "1 machine"
        : `${nodeCount} machines`;

  return (
    <Page
      title="Settings"
      subtitle="Storage, backups and the machines everything runs on."
    >
      <Card className="mb-4 overflow-hidden p-0">
        <NavRow
          to="/box/storage"
          icon={Database}
          label="Storage"
          detail={storageDetail}
          cache={storage.cache}
          tone={
            health.data?.level === "error"
              ? "error"
              : health.data?.level === "warn"
                ? "warn"
                : undefined
          }
        />
        <NavRow
          to="/box/backups"
          icon={Cloud}
          label="Backups"
          cache={backups.cache}
          detail={
            backups.data === undefined
              ? undefined
              : backups.data.configured
                ? "Turned on"
                : "Not set up yet — your files are not backed up"
          }
          tone={backups.data && !backups.data.configured ? "warn" : undefined}
        />
        <NavRow
          to="/box/machines"
          icon={Server}
          label="Machines"
          detail={nodesDetail}
          cache={nodes.cache}
        />
        <NavRow
          to="/box/system"
          icon={Wrench}
          label="Updates and system"
          detail={status.data?.platform}
        />
        {/* Only when the backend worked out where the console is. Rendering it
            unconditionally would mean a box whose config has no platform API
            shows a link that goes nowhere.

            A click handler rather than an href, because the real URL carries
            the account token in its fragment and is fetched at the moment of
            the click. As an href it would sit in the DOM — and in the page
            source, and in anything that scrapes it — from first paint. */}
        {status.data?.console_url && (
          <NavRow
            onClick={openConsole}
            icon={CreditCard}
            label="Account and billing"
            detail="Your plan, invoices and payment details"
          />
        )}
      </Card>

      <Card className="mb-4 p-5">
        <div className="mb-3 text-sm font-medium text-fg">Appearance</div>
        <div className="flex gap-1 rounded-xl bg-surface-2 p-1">
          {THEMES.map((t) => (
            <button
              key={t.id}
              onClick={() => setTheme(t.id)}
              className={cn(
                "flex-1 rounded-lg px-3 py-2 text-sm transition-colors",
                choice === t.id
                  ? "bg-surface font-medium text-fg shadow-[var(--shadow-card)]"
                  : "text-fg-muted hover:text-fg",
              )}
            >
              {t.label}
            </button>
          ))}
        </div>
      </Card>

      <h2 className="mb-2 mt-8 px-1 text-sm font-semibold text-fg-muted">
        Advanced
      </h2>
      <p className="mb-3 px-1 text-sm text-fg-subtle">
        You should not need these. They are here for when something has gone
        wrong and someone is helping you.
      </p>
      <Card className="overflow-hidden p-0">
        {/* Above Terminal on purpose. Reading what the machine already said
            should be the first thing reached for when something is wrong, and
            it is the one entry here that is safe to open out of curiosity. */}
        <NavRow
          to="/box/logs"
          icon={ScrollText}
          label="Logs"
          detail="What the machine has been saying"
        />
        <NavRow
          to="/box/terminal"
          icon={TerminalSquare}
          label="Terminal"
          detail="Run commands on the machine"
        />
      </Card>
    </Page>
  );
}
