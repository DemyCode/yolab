import type { ReactNode } from "react";
import { Link, useOutletContext } from "react-router-dom";
import {
  ChevronRight,
  Cloud,
  CreditCard,
  Database,
  ExternalLink,
  LogOut,
  Server,
  ScrollText,
  TerminalSquare,
  Wrench,
} from "lucide-react";
import { Page } from "@/components/AppShell";
import { Card } from "@/components/ui/card";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import { RollingNumber } from "@/components/motion";
import { MovementSummary } from "@/components/DataMovement";
import { isVisible, useMovement } from "@/lib/movement";
import { ThemeControl } from "@/components/ThemeControl";
import { creditStatus, formatBytes, formatEuros } from "@/lib/format";
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
  external,
}: {
  to?: string;
  href?: string;
  onClick?: () => void;
  icon: typeof Database;
  label: string;
  detail?: ReactNode;
  tone?: "warn" | "error";
  external?: boolean;
}) {
  const inner = (
    <>
      <div
        className={cn(
          "flex h-10 w-10 shrink-0 items-center justify-center rounded-control",
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
          <div
            className={cn(
              "mt-0.5 flex items-center gap-1.5 text-sm text-fg-muted",
              !tone && "truncate",
            )}
          >
            <span className={cn(!tone && "truncate")}>{detail}</span>
          </div>
        )}
      </div>
      {href || external ? (
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

export function SystemPage() {
  const { signOut } = useOutletContext<{ signOut: () => void }>();

  const health = useApi<ClusterHealth>("health", "/api/cluster/health");
  const movement = useMovement();
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
  const billing = useApi<{
    credit_cents: number;
    suspended: boolean;
    stops_on: string | null;
  }>(
    status.data?.console_url ? "billing-credit" : null,
    "/api/billing/balance",
  );
  const credit = billing.data ? creditStatus(billing.data) : undefined;
  const creditDetail =
    billing.data && credit ? (
      <>
        <RollingNumber value={billing.data.credit_cents} format={formatEuros} />
        {credit.text.slice(formatEuros(billing.data.credit_cents).length)}
      </>
    ) : undefined;

  const detail = storage.data?.data;
  const storageDetail = detail ? (
    <>
      <RollingNumber value={detail.used_bytes} format={formatBytes} /> used of{" "}
      <RollingNumber value={detail.total_bytes} format={formatBytes} />
    </>
  ) : health.data?.starting ? (
    "Starting up…"
  ) : undefined;

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
    nodeCount === 0 ? undefined : (
      <>
        <RollingNumber value={nodeCount} />{" "}
        {nodeCount === 1 ? "machine" : "machines"}
      </>
    );

  return (
    <Page
      title="System"
      subtitle="Storage, backups and the machines everything runs on."
    >
      <Card className="mb-4 overflow-hidden p-0">
        <NavRow
          to="/system/storage"
          icon={Database}
          label="Storage"
          detail={
            isVisible(movement.data) ? <MovementSummary /> : storageDetail
          }
          tone={
            health.data?.level === "error"
              ? "error"
              : health.data?.level === "warn"
                ? "warn"
                : undefined
          }
        />
        <NavRow
          to="/system/backups"
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
          to="/system/machines"
          icon={Server}
          label="Machines"
          detail={nodesDetail}
        />
        <NavRow
          to="/system/updates"
          icon={Wrench}
          label="Updates"
          detail={status.data?.platform}
        />
        {status.data?.console_url && (
          <NavRow
            onClick={openConsole}
            icon={CreditCard}
            label="Account and billing"
            detail={creditDetail ?? "Your credit, top-ups and referral code"}
            tone={credit?.tone}
            external
          />
        )}
      </Card>

      <Card className="mb-4 p-5">
        <div className="mb-3 text-sm font-medium text-fg">Appearance</div>
        <ThemeControl />
      </Card>

      <h2 className="mb-2 mt-8 px-1 text-sm font-semibold text-fg-muted">
        Advanced
      </h2>
      <p className="mb-3 px-1 text-sm text-fg-subtle">
        You should not need these. They are here for when something has gone
        wrong and someone is helping you.
      </p>
      <Card className="mb-4 overflow-hidden p-0">
        <NavRow
          to="/system/logs"
          icon={ScrollText}
          label="Logs"
          detail="What the machine has been saying"
        />
        <NavRow
          to="/system/terminal"
          icon={TerminalSquare}
          label="Terminal"
          detail="Run commands on the machine"
        />
      </Card>

      <Card className="overflow-hidden p-0">
        <NavRow
          onClick={() => void signOut()}
          icon={LogOut}
          label="Sign out"
          detail="Sign out of your server on this device"
        />
      </Card>
    </Page>
  );
}
