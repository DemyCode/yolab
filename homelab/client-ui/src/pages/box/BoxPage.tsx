import { Link } from "react-router-dom";
import {
  ChevronRight,
  Cloud,
  Database,
  ExternalLink,
  Gauge,
  Server,
  TerminalSquare,
  Wrench,
} from "lucide-react";
import { Page } from "@/components/AppShell";
import { Card } from "@/components/ui/card";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";
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
  icon: Icon,
  label,
  detail,
  tone,
}: {
  to?: string;
  href?: string;
  icon: typeof Database;
  label: string;
  detail?: string;
  tone?: "warn" | "error";
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
          <div className="mt-0.5 truncate text-sm text-fg-muted">{detail}</div>
        )}
      </div>
      {href ? (
        <ExternalLink className="h-4 w-4 shrink-0 text-fg-subtle" />
      ) : (
        <ChevronRight className="h-5 w-5 shrink-0 text-fg-subtle" />
      )}
    </>
  );

  const className =
    "flex items-center gap-4 px-5 py-4 transition-colors hover:bg-surface-2 border-b border-border last:border-0";

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

  const health = useResource<ClusterHealth>("health", () =>
    api.get("/api/cluster/health"),
  );
  const storage = useResource<StorageDetailResponse>("storage-detail", () =>
    api.get("/api/ceph/detail"),
  );
  const nodes = useResource<NodeInfo[]>("nodes", () => api.get("/api/nodes"));
  const status = useResource<StatusInfo>("status", () =>
    api.get("/api/status"),
  );
  const backups = useResource<{ configured: boolean }>("backups-s3", () =>
    api.get("/api/backups/s3"),
  );

  const detail = storage.data?.data;
  const storageDetail = detail
    ? `${formatBytes(detail.used_bytes)} used of ${formatBytes(detail.total_bytes)}`
    : health.data?.starting
      ? "Starting up…"
      : undefined;

  const nodeCount = nodes.data?.length ?? 0;
  const nodesDetail =
    nodeCount === 0
      ? undefined
      : nodeCount === 1
        ? "1 machine"
        : `${nodeCount} machines`;

  return (
    <Page
      title="Your box"
      subtitle="Settings for the machine everything runs on."
    >
      <Card className="mb-4 overflow-hidden p-0">
        <NavRow
          to="/box/storage"
          icon={Database}
          label="Storage"
          detail={storageDetail}
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
        />
        <NavRow
          to="/box/system"
          icon={Wrench}
          label="Updates and system"
          detail={status.data?.platform}
        />
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
        <NavRow
          to="/box/terminal"
          icon={TerminalSquare}
          label="Terminal"
          detail="Run commands on the machine"
        />
        <NavRow
          href="/glances/"
          icon={Gauge}
          label="System monitor"
          detail="Live CPU, memory and network"
        />
      </Card>
    </Page>
  );
}
