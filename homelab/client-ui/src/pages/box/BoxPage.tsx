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
  onClick?: () => void;
  icon: typeof Database;
  label: string;
  detail?: string;
  tone?: "warn" | "error";
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
  const storage = useApi<StorageDetailResponse>(
    "storage-detail",
    "/api/ceph/detail",
  );
  const nodes = useApi<NodeInfo[]>("nodes", "/api/nodes");
  const status = useApi<StatusInfo>("status", "/api/status");
  const backups = useApi<{ configured: boolean }>(
    "backups-s3",
    "/api/backups/s3",
  );

  const detail = storage.data?.data;
  const storageDetail = detail
    ? `${formatBytes(detail.used_bytes)} used of ${formatBytes(detail.total_bytes)}`
    : health.data?.starting
      ? "Starting up…"
      : undefined;

  async function openConsole() {
    const w = window.open("about:blank", "_blank");

    const go = (url: string) => {
      if (w) {
        w.opener = null;
        w.location.replace(url);
      } else {
        window.location.assign(url);
      }
    };

    try {
      const { url } = await api.get<{ url: string }>("/api/console/link");
      go(url);
    } catch {
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
        {

}
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
        {
}
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
