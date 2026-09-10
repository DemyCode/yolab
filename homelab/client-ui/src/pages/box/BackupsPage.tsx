import { useEffect, useState, useCallback } from "react";
import {
  Database,
  RefreshCw,
  CheckCircle,
  AlertCircle,
  AlertTriangle,
  Circle,
  RotateCcw,
  KeyRound,
  Copy,
} from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import type { ClusterHealth } from "@/types/health";
import { Button } from "@/components/ui/button";

// ── Types ─────────────────────────────────────────────────────────────────────

interface PvcEntry {
  name: string;
  capacity: string;
}

interface ServiceEntry {
  namespace: string;
  pvcs: PvcEntry[];
  app_id?: string;
  instance_name?: string;
  images?: string[];
}

interface SnapshotCatalog {
  timestamp: string;
  namespaces: string[];
  services?: ServiceEntry[];
  catalog_version?: string | null;
}

interface DiffEntry {
  namespace: string;
  serviceName: string;
  appId?: string;
  pvcs: PvcEntry[];
  mode: "adding" | "recovering";
}

// Mirrors backup.rs's `list()` — one record per backup set, in three states.
type BackupSetState = "running" | "restorable" | "crashed";

interface BackupSet {
  id: string;
  triggered_by: string;
  started_at: string;
  finished_at?: string | null;
  snapshot_id?: string | null;
  error?: string | null;
  state: BackupSetState;
}

interface OperationState {
  backing_up: boolean;
  restoring: boolean;
  backup_run: BackupSet | null;
  restore_run: RestoreRunStatus | null;
  last_backup: BackupSet | null;
  last_ok_age_hours: number | null;
  stale_after_hours: number;
}

interface RecoveryKeyResponse {
  configured: boolean;
  recovery_key?: string;
}

// Mirrors restore_run.rs's RestoreRun.status shape.
interface VolumeStatus {
  pvc: string;
  phase: "Pending" | "Deleting" | "Restoring" | "Succeeded" | "Failed" | "Skipped";
}

interface DeploymentScale {
  name: string;
  replicas: number;
}

interface NamespaceRestoreStatus {
  namespace: string;
  scaledDeployments: DeploymentScale[];
  volumes: VolumeStatus[];
  setupComplete?: boolean;
}

interface RestoreRunStatus {
  phase:
    | "Validating"
    | "RebuildingStorage"
    | "WaitingForStorage"
    | "RestoringVolumes"
    | "Applying"
    | "Succeeded"
    | "Partial"
    | "Failed";
  startedAt?: string;
  finishedAt?: string;
  error?: string | null;
  snapshotId?: string;
  restoreAsOf?: string | null;
  namespaces?: NamespaceRestoreStatus[];
  abortReason?: string | null;
  restoredFromVersion?: string | null;
}

interface DrStatusResponse {
  active: RestoreRunStatus | null;
  last: RestoreRunStatus | null;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function Shimmer({ className }: { className?: string }) {
  return (
    <div className={`animate-pulse rounded bg-border ${className ?? ""}`} />
  );
}

function serviceNameFromNamespace(ns: string): string {
  const s = ns.replace(/^yolab-/, "");
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function formatDate(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function timeAgo(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const h = Math.floor(diff / 3600000);
  const m = Math.floor((diff % 3600000) / 60000);
  const d = Math.floor(h / 24);
  if (d > 0) return `${d}d ago`;
  if (h > 0) return `${h}h ago`;
  return `${m}m ago`;
}

// ── Restore takeover — full-page while a RestoreRun is active ─────────────────

const RESTORE_PHASES: { key: RestoreRunStatus["phase"]; label: string }[] = [
  { key: "Validating", label: "Validating snapshot" },
  { key: "RebuildingStorage", label: "Rebuilding storage" },
  { key: "WaitingForStorage", label: "Waiting for storage" },
  { key: "RestoringVolumes", label: "Restoring volumes" },
  { key: "Applying", label: "Bringing services back up" },
];

function isTerminalRestorePhase(phase: string): boolean {
  return phase === "Succeeded" || phase === "Partial" || phase === "Failed";
}

function VolumePhaseIcon({ phase }: { phase: VolumeStatus["phase"] }) {
  switch (phase) {
    case "Succeeded":
      return <CheckCircle className="h-4 w-4 text-success flex-shrink-0" />;
    case "Failed":
      return <AlertCircle className="h-4 w-4 text-danger flex-shrink-0" />;
    case "Skipped":
      return <AlertTriangle className="h-4 w-4 text-warning flex-shrink-0" />;
    case "Deleting":
    case "Restoring":
      return (
        <RefreshCw className="h-4 w-4 text-primary animate-spin flex-shrink-0" />
      );
    default:
      return <Circle className="h-4 w-4 text-border-strong flex-shrink-0" />;
  }
}

function volumePhaseLabel(phase: VolumeStatus["phase"]): string {
  switch (phase) {
    case "Succeeded":
      return "Restored";
    case "Failed":
      return "Failed";
    case "Skipped":
      return "No backup found — kept as-is";
    case "Deleting":
      return "Clearing old volume…";
    case "Restoring":
      return "Restoring…";
    default:
      return "Pending";
  }
}

function RestoreTakeover({ onDone }: { onDone: () => void }) {
  const [status, setStatus] = useState<RestoreRunStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    async function poll() {
      try {
        const data = (await fetch("/api/backups/dr/status").then((r) =>
          r.json(),
        )) as DrStatusResponse;
        if (cancelled) return;
        setStatus(data.active ?? data.last ?? null);
      } catch {
        /* network blip */
      }
    }
    void poll();
    const id = window.setInterval(poll, 3000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  if (!status) {
    return (
      <div className="flex flex-col items-center justify-center min-h-[60vh] gap-3">
        <RefreshCw className="h-6 w-6 text-primary animate-spin" />
        <p className="text-sm text-fg-muted">Starting restore…</p>
      </div>
    );
  }

  const terminal = isTerminalRestorePhase(status.phase);
  const currentIndex = RESTORE_PHASES.findIndex((p) => p.key === status.phase);
  const namespaces = status.namespaces ?? [];
  const totalVolumes = namespaces.reduce((n, ns) => n + ns.volumes.length, 0);
  const succeededVolumes = namespaces.reduce(
    (n, ns) => n + ns.volumes.filter((v) => v.phase === "Succeeded").length,
    0,
  );

  return (
    <div className="min-h-[70vh] flex flex-col max-w-3xl mx-auto w-full">
      <div className="mb-8">
        <h2 className="font-display text-2xl text-fg">Restoring from backup</h2>
        <p className="text-sm text-fg-muted mt-0.5">
          {status.snapshotId
            ? `Snapshot ${status.snapshotId.slice(0, 8)}`
            : "Restore in progress"}
          {status.restoreAsOf
            ? ` · as of ${formatDate(status.restoreAsOf)}`
            : ""}
          {" — "}other backup and restore actions are disabled until this
          finishes.
        </p>
      </div>

      {!terminal && (
        <div className="flex items-center mb-8">
          {RESTORE_PHASES.map((p, i) => (
            <div
              key={p.key}
              className="flex items-center flex-1 last:flex-none"
            >
              <div className="flex flex-col items-center gap-2">
                <div
                  className={`h-8 w-8 rounded-full flex items-center justify-center border-2 ${
                    i < currentIndex
                      ? "border-success bg-success/10"
                      : i === currentIndex
                        ? "border-primary bg-primary/10"
                        : "border-border-strong"
                  }`}
                >
                  {i < currentIndex ? (
                    <CheckCircle className="h-4 w-4 text-success" />
                  ) : i === currentIndex ? (
                    <RefreshCw className="h-4 w-4 text-primary animate-spin" />
                  ) : (
                    <span className="text-xs text-fg-subtle">{i + 1}</span>
                  )}
                </div>
                <span
                  className={`text-xs whitespace-nowrap ${i === currentIndex ? "text-fg font-medium" : "text-fg-subtle"}`}
                >
                  {p.label}
                </span>
              </div>
              {i < RESTORE_PHASES.length - 1 && (
                <div
                  className={`flex-1 h-px mx-2 ${i < currentIndex ? "bg-success" : "bg-border-strong"}`}
                />
              )}
            </div>
          ))}
        </div>
      )}

      {terminal && (
        <div
          className={`rounded-lg border px-4 py-3 mb-8 flex items-start gap-2 ${
            status.phase === "Succeeded"
              ? "border-success-soft bg-success-soft"
              : status.phase === "Partial"
                ? "border-warning-soft bg-warning-soft"
                : "border-danger-soft bg-danger-soft"
          }`}
        >
          {status.phase === "Succeeded" ? (
            <CheckCircle className="h-4 w-4 text-success flex-shrink-0 mt-0.5" />
          ) : (
            <AlertTriangle
              className={`h-4 w-4 flex-shrink-0 mt-0.5 ${status.phase === "Partial" ? "text-warning" : "text-danger"}`}
            />
          )}
          <div className="text-sm">
            <p
              className={`font-medium ${
                status.phase === "Succeeded"
                  ? "text-success"
                  : status.phase === "Partial"
                    ? "text-warning"
                    : "text-danger"
              }`}
            >
              {status.phase === "Succeeded" &&
                `Restore complete — ${succeededVolumes}/${totalVolumes || 0} volume${totalVolumes === 1 ? "" : "s"} restored.`}
              {status.phase === "Partial" &&
                `Restore finished with issues — ${succeededVolumes}/${totalVolumes} volumes restored. Affected services are running with their previous or empty data.`}
              {status.phase === "Failed" &&
                `Restore failed${status.error ? `: ${status.error}` : "."}`}
            </p>
            {status.abortReason && (
              <p className="text-xs text-fg-muted mt-1">
                {status.abortReason} — services were scaled back up
                automatically.
              </p>
            )}
          </div>
        </div>
      )}

      {namespaces.length > 0 && (
        <div className="space-y-3 flex-1">
          {namespaces.map((ns) => (
            <Card key={ns.namespace} className="border-border">
              <CardContent className="pt-4 pb-4">
                <p className="text-sm font-medium text-fg mb-2">
                  {serviceNameFromNamespace(ns.namespace)}
                </p>
                {ns.volumes.length === 0 ? (
                  <p className="text-xs text-fg-subtle">
                    No volumes — configuration restored only.
                  </p>
                ) : (
                  <div className="space-y-1.5">
                    {ns.volumes.map((v) => (
                      <div
                        key={v.pvc}
                        className="flex items-center gap-2 text-xs"
                      >
                        <VolumePhaseIcon phase={v.phase} />
                        <span className="text-fg-muted font-mono">{v.pvc}</span>
                        <span className="text-fg-subtle ml-auto">
                          {volumePhaseLabel(v.phase)}
                        </span>
                      </div>
                    ))}
                  </div>
                )}
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      {terminal && (
        <div className="flex justify-end mt-6">
          <Button
            onClick={onDone}
            className="h-9 px-4 text-sm bg-primary hover:bg-primary text-bg font-medium"
          >
            Back to Backups
          </Button>
        </div>
      )}
    </div>
  );
}

// ── Restore flow (confirm step) ───────────────────────────────────────────────

function RestoreFlow({
  snapshotId,
  snapshotTime,
  catalog,
  runningNamespaces,
  onCancel,
  onStarted,
}: {
  snapshotId: string;
  snapshotTime: string;
  catalog: SnapshotCatalog;
  runningNamespaces: Set<string>;
  onCancel: () => void;
  onStarted: () => void;
}) {
  const [selected, setSelected] = useState<Set<string>>(
    () => new Set(catalog.namespaces),
  );
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const health = useResource<ClusterHealth>("health", () =>
    api.get<ClusterHealth>("/api/cluster/health"),
  );
  const storageUnrecoverable = health.data?.storage_unrecoverable ?? false;

  const services: ServiceEntry[] =
    catalog.services ??
    catalog.namespaces.map((ns) => ({ namespace: ns, pvcs: [] }));

  const diff: DiffEntry[] = services.map((svc) => ({
    namespace: svc.namespace,
    serviceName: serviceNameFromNamespace(svc.namespace),
    appId:
      svc.app_id && svc.app_id !== svc.instance_name ? svc.app_id : undefined,
    pvcs: svc.pvcs,
    mode: runningNamespaces.has(svc.namespace) ? "recovering" : "adding",
  }));

  const addingCount = diff.filter(
    (e) => e.mode === "adding" && selected.has(e.namespace),
  ).length;
  const recoveringCount = diff.filter(
    (e) => e.mode === "recovering" && selected.has(e.namespace),
  ).length;

  function toggle(ns: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(ns)) {
        next.delete(ns);
      } else {
        next.add(ns);
      }
      return next;
    });
  }

  async function handleAccept() {
    setError(null);
    setStarting(true);
    try {
      const res = await fetch("/api/backups/dr/start", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          snapshot_id: snapshotId,
          namespaces: [...selected],
          rebuild_storage: storageUnrecoverable,
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      onStarted();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
      setStarting(false);
    }
  }

  return (
    <div className="border border-border-strong rounded-lg p-4 space-y-4 bg-surface">
      <div className="flex items-center justify-between gap-2">
        <p className="text-sm font-semibold text-fg">
          Restore from {formatDate(snapshotTime)}
        </p>
        <button
          onClick={onCancel}
          className="text-xs text-fg-subtle hover:text-fg-muted"
        >
          ✕ Cancel
        </button>
      </div>

      <div className="space-y-2">
        {diff.map((entry) => (
          <label
            key={entry.namespace}
            className="flex items-start gap-3 cursor-pointer"
          >
            <input
              type="checkbox"
              checked={selected.has(entry.namespace)}
              onChange={() => toggle(entry.namespace)}
              className="mt-0.5 h-4 w-4 rounded border-border-strong bg-surface-2 accent-primary"
            />
            <div className="flex-1 min-w-0">
              <div className="flex items-center gap-2">
                <span className="text-sm text-fg font-medium">
                  {entry.serviceName}
                </span>
                {entry.appId && (
                  <span className="text-xs text-fg-subtle">{entry.appId}</span>
                )}
                {entry.mode === "adding" ? (
                  <span className="text-xs text-success font-medium">
                    Adding
                  </span>
                ) : (
                  <span className="text-xs text-warning font-medium">
                    Recovering
                  </span>
                )}
              </div>
              {entry.pvcs.length > 0 && (
                <p className="text-xs text-fg-subtle mt-0.5">
                  {entry.pvcs
                    .map((p) => `${p.name} (${p.capacity})`)
                    .join(" · ")}
                </p>
              )}
            </div>
          </label>
        ))}
      </div>

      {storageUnrecoverable && (
        <div className="rounded border border-danger-soft bg-danger-soft px-3 py-2 text-xs text-danger space-y-1 mb-2">
          <p className="font-medium flex items-center gap-1.5">
            <AlertTriangle className="h-3.5 w-3.5 flex-shrink-0" />
            Your storage is damaged and cannot be repaired
          </p>
          <p>
            A disk was lost and what it held is not stored anywhere else, so it
            cannot be rebuilt. This restore will clear the damaged storage, set
            it up again, and put your backup back — in one go. Anything changed
            since this backup was taken will not come back.
          </p>
          <p>
            If that disk still works, reconnecting it instead recovers
            everything with nothing lost. That is the better option, if you have
            it.
          </p>
        </div>
      )}

      {selected.size > 0 && (
        <div className="rounded border border-danger-soft bg-danger-soft px-3 py-2 text-xs text-danger space-y-0.5">
          <p className="font-medium flex items-center gap-1.5">
            <AlertTriangle className="h-3.5 w-3.5 flex-shrink-0" />
            This cannot be undone — current data will be replaced.
          </p>
          {addingCount > 0 && (
            <p>
              · {addingCount} service{addingCount !== 1 ? "s" : ""} will be
              created from backup.
            </p>
          )}
          {recoveringCount > 0 && (
            <p>
              · {recoveringCount} running service
              {recoveringCount !== 1 ? "s" : ""} will be stopped and restored.
            </p>
          )}
        </div>
      )}

      {error && <p className="text-xs text-danger">{error}</p>}

      <div className="flex justify-end gap-2">
        <Button
          variant="outline"
          onClick={onCancel}
          disabled={starting}
          className="h-8 px-3 text-xs border-border-strong text-fg-muted hover:text-fg"
        >
          Cancel
        </Button>
        <Button
          onClick={handleAccept}
          disabled={selected.size === 0 || starting}
          className="h-8 px-4 text-xs bg-danger hover:bg-danger text-white border-0 font-medium disabled:opacity-40"
        >
          {starting ? (
            <>
              <RefreshCw className="h-3 w-3 mr-1.5 animate-spin" />
              Starting…
            </>
          ) : (
            `Accept & Restore (${selected.size})`
          )}
        </Button>
      </div>
    </div>
  );
}

// ── One backup set ────────────────────────────────────────────────────────────

function setStateLabel(state: BackupSetState): string {
  switch (state) {
    case "running":
      return "Backing up now";
    case "restorable":
      return "Restorable";
    case "crashed":
      return "Incomplete";
  }
}

function BackupSetCard({
  set: backupSet,
  runningNamespaces,
  disabled,
  onRestoreStarted,
}: {
  set: BackupSet;
  runningNamespaces: Set<string>;
  disabled: boolean;
  onRestoreStarted: () => void;
}) {
  const [catalog, setCatalog] = useState<SnapshotCatalog | null>(null);
  const [restoring, setRestoring] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const isRunning = backupSet.state === "running";
  const isRestorable = backupSet.state === "restorable";

  async function handleRestoreClick() {
    if (!backupSet.snapshot_id) return;
    setRestoring(true);
    setLoading(true);
    setError(null);
    try {
      const data = (await fetch(
        `/api/backups/snapshots/${backupSet.snapshot_id}/catalog`,
      ).then((r) => r.json())) as SnapshotCatalog;
      setCatalog(data);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load");
      setRestoring(false);
    } finally {
      setLoading(false);
    }
  }

  const when = backupSet.started_at;

  return (
    <Card className={isRunning ? "border-primary/30 bg-primary-soft/20" : "border-border"}>
      <CardContent className="pt-4 pb-4">
        <div className="flex items-center gap-3">
          {isRunning ? (
            <RefreshCw className="h-4 w-4 text-primary flex-shrink-0 animate-spin" />
          ) : isRestorable ? (
            <CheckCircle className="h-4 w-4 text-success flex-shrink-0" />
          ) : (
            <AlertTriangle className="h-4 w-4 text-warning flex-shrink-0" />
          )}
          <div className="flex-1 min-w-0">
            <span className="text-sm font-medium text-fg">
              {formatDate(when)}
            </span>
            <span className="ml-2 text-xs text-fg-subtle">{timeAgo(when)}</span>
            <span
              className={`ml-2 text-xs ${isRunning ? "text-primary" : isRestorable ? "text-success" : "text-warning"}`}
            >
              {setStateLabel(backupSet.state)}
            </span>
            {backupSet.triggered_by === "schedule" && (
              <span className="ml-2 text-xs text-fg-muted">· automatic</span>
            )}
          </div>
          {!restoring && isRestorable && !disabled && (
            <Button
              onClick={handleRestoreClick}
              variant="outline"
              className="flex-shrink-0 h-7 px-3 text-xs border-border-strong text-primary hover:border-primary hover:text-primary disabled:opacity-30"
            >
              Restore from here
            </Button>
          )}
        </div>

        {!isRunning && backupSet.error && (
          <p className="mt-2 text-xs text-danger">{backupSet.error}</p>
        )}

        {loading && <Shimmer className="mt-3 h-12 w-full" />}
        {error && <p className="mt-3 text-xs text-danger">{error}</p>}

        {restoring && catalog && (
          <div className="mt-3">
            <RestoreFlow
              snapshotId={backupSet.snapshot_id!}
              snapshotTime={backupSet.started_at}
              catalog={catalog}
              runningNamespaces={runningNamespaces}
              onCancel={() => {
                setRestoring(false);
                setCatalog(null);
              }}
              onStarted={onRestoreStarted}
            />
          </div>
        )}
      </CardContent>
    </Card>
  );
}

// ── Recovery key overlay ──────────────────────────────────────────────────────

function RecoveryKeyOverlay({
  recoveryKey,
  mandatory,
  onClose,
}: {
  recoveryKey: string;
  mandatory: boolean;
  onClose: () => void;
}) {
  const [acknowledged, setAcknowledged] = useState(false);
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(recoveryKey);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      /* clipboard unavailable — user can still select-and-copy */
    }
  }

  return (
    <div className="fixed inset-0 z-50 bg-bg/95 backdrop-blur-sm flex items-center justify-center p-6">
      <div className="max-w-lg w-full border border-border-strong rounded-lg bg-surface p-6 space-y-4">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0 bg-warning-soft">
            <KeyRound className="h-4 w-4 text-warning" strokeWidth={1.75} />
          </div>
          <div>
            <p className="text-sm font-semibold text-fg">
              Your backup recovery key
            </p>
            <p className="text-xs text-fg-muted mt-1">
              This is the only way to decrypt your backups if this machine is
              lost or destroyed. YoLab does not store a copy anywhere else. Save
              it now in a password manager or print it — without it, your
              backups on Backblaze B2 are permanently unreadable.
            </p>
          </div>
        </div>

        <div className="flex items-center gap-2">
          <code className="flex-1 text-sm font-mono text-fg bg-surface-2 border border-border-strong rounded px-3 py-2 break-all select-all">
            {recoveryKey}
          </code>
          <Button
            onClick={handleCopy}
            variant="outline"
            className="flex-shrink-0 h-9 px-3 text-xs border-border-strong text-fg-muted hover:text-fg"
          >
            {copied ? (
              <CheckCircle className="h-3.5 w-3.5 text-success" />
            ) : (
              <Copy className="h-3.5 w-3.5" />
            )}
          </Button>
        </div>

        {mandatory && (
          <label className="flex items-start gap-2 cursor-pointer">
            <input
              type="checkbox"
              checked={acknowledged}
              onChange={() => setAcknowledged((a) => !a)}
              className="mt-0.5 h-4 w-4 rounded border-border-strong bg-surface-2 accent-primary"
            />
            <span className="text-xs text-fg-muted">
              I've saved this recovery key somewhere safe and durable.
            </span>
          </label>
        )}

        <div className="flex justify-end">
          <Button
            onClick={onClose}
            disabled={mandatory && !acknowledged}
            className="h-8 px-4 text-xs bg-primary hover:bg-primary text-bg font-medium disabled:opacity-40"
          >
            {mandatory ? "I've saved it — continue" : "Close"}
          </Button>
        </div>
      </div>
    </div>
  );
}

// ── Enable card ───────────────────────────────────────────────────────────────

function EnableCard({
  onEnable,
  disabled,
}: {
  onEnable: () => Promise<void>;
  disabled: boolean;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handle() {
    setBusy(true);
    setError(null);
    try {
      await onEnable();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0 bg-warning-soft">
            <Database className="h-4 w-4 text-warning" strokeWidth={1.75} />
          </div>
          <div className="flex-1">
            <div className="flex items-center justify-between gap-4 flex-wrap">
              <div>
                <p className="text-sm font-medium text-fg">
                  Backups not configured
                </p>
                <p className="text-xs text-fg-muted mt-0.5">
                  Enable to start daily encrypted backups to Backblaze B2
                </p>
              </div>
              <Button
                onClick={handle}
                disabled={busy || disabled}
                className="bg-primary hover:bg-primary text-bg font-medium text-sm h-8 px-3 disabled:opacity-40"
              >
                {busy ? (
                  <>
                    <RefreshCw className="h-3.5 w-3.5 mr-1.5 animate-spin" />
                    Enabling…
                  </>
                ) : (
                  "Enable Backups"
                )}
              </Button>
            </div>
            {error && <p className="mt-2 text-xs text-danger">{error}</p>}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

export function BackupsPage() {
  const [s3Status, setS3Status] = useState<{ provisioned: boolean } | null>(
    null,
  );
  const [runningNamespaces, setRunning] = useState<Set<string>>(new Set());
  const [sets, setSets] = useState<BackupSet[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [opState, setOpState] = useState<OperationState>({
    backing_up: false,
    restoring: false,
    backup_run: null,
    restore_run: null,
    last_backup: null,
    last_ok_age_hours: null,
    stale_after_hours: 24,
  });
  const [recoveryKey, setRecoveryKey] = useState<string | null>(null);
  const [recoveryMandatory, setRecoveryMandatory] = useState(false);
  const [showRestoreView, setShowRestoreView] = useState(false);

  async function showRecoveryKey(mandatory: boolean) {
    try {
      const data = (await fetch("/api/backups/recovery-key").then((r) =>
        r.json(),
      )) as RecoveryKeyResponse;
      if (data.configured && data.recovery_key) {
        setRecoveryKey(data.recovery_key);
        setRecoveryMandatory(mandatory);
      }
    } catch {
      /* network blip — user can retry via "View recovery key" */
    }
  }

  const load = useCallback(async () => {
    const [s3Res, statusRes, runsRes] = await Promise.all([
      fetch("/api/backups/s3")
        .then((r) => r.json())
        .catch(() => ({ provisioned: false })),
      fetch("/api/backups/status")
        .then((r) => r.json())
        .catch(() => null),
      fetch("/api/backups/runs")
        .then((r) => r.json())
        .catch(() => []),
    ]);
    setS3Status(s3Res as { provisioned: boolean });

    const status = statusRes as {
      pvcs?: { namespace: string }[];
    } | null;
    if (status?.pvcs) {
      setRunning(new Set(status.pvcs.map((p) => p.namespace)));
    }

    if (Array.isArray(runsRes)) {
      setSets(runsRes as BackupSet[]);
    }
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const pollOpState = useCallback(async () => {
    try {
      const s = (await fetch("/api/backups/state").then((r) =>
        r.json(),
      )) as OperationState;
      setOpState(s);
      return s;
    } catch {
      return null;
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    const id = window.setInterval(() => {
      if (!cancelled) void pollOpState();
    }, 5000);
    void pollOpState();
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [pollOpState]);

  useEffect(() => {
    if (opState.restoring) setShowRestoreView(true);
  }, [opState.restoring]);

  const opBusy = opState.backing_up || opState.restoring;

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok)
      throw new Error((await res.text()) || `Server error ${res.status}`);
    await load();
    await showRecoveryKey(true);
  }

  const [backingUp, setBackingUp] = useState(false);
  const [backupError, setBackupError] = useState<string | null>(null);
  async function handleBackupNow() {
    setBackingUp(true);
    setBackupError(null);
    try {
      const res = await fetch("/api/backups/cluster/run-now", {
        method: "POST",
      });
      if (!res.ok) throw new Error(await res.text());
      await pollOpState();
      await load();
    } catch (e) {
      setBackupError(e instanceof Error ? e.message : "Backup failed");
    } finally {
      setBackingUp(false);
    }
  }

  if (showRestoreView) {
    return (
      <RestoreTakeover
        onDone={() => {
          setShowRestoreView(false);
          void pollOpState();
          void load();
        }}
      />
    );
  }

  return (
    <div className="space-y-6 max-w-3xl">
      {recoveryKey && (
        <RecoveryKeyOverlay
          recoveryKey={recoveryKey}
          mandatory={recoveryMandatory}
          onClose={() => setRecoveryKey(null)}
        />
      )}

      <div className="flex items-start justify-between gap-4 flex-wrap">
        <div>
          <p className="text-sm text-fg-muted mt-0.5">
            Each backup is a full snapshot of the cluster — K8s state, service
            configs, and all PVC data — encrypted and stored in Backblaze B2.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {s3Status?.provisioned && (
            <button
              onClick={() => void showRecoveryKey(false)}
              className="flex-shrink-0 flex items-center gap-1.5 text-xs text-fg-muted hover:text-fg"
            >
              <KeyRound className="h-3.5 w-3.5" />
              View recovery key
            </button>
          )}
          {s3Status?.provisioned && (
            <Button
              onClick={handleBackupNow}
              disabled={backingUp}
              variant="outline"
              className="flex-shrink-0 h-8 px-3 text-xs border-border-strong text-fg-muted hover:text-fg disabled:opacity-40"
            >
              {backingUp ? (
                <>
                  <RefreshCw className="h-3 w-3 mr-1.5 animate-spin" />
                  Backing up…
                </>
              ) : (
                <>
                  <RotateCcw className="h-3 w-3 mr-1.5" />
                  Backup Now
                </>
              )}
            </Button>
          )}
        </div>
      </div>

      {backupError && <p className="text-xs text-danger">{backupError}</p>}

      {opState.backing_up && (
        <div className="rounded-lg border border-warning-soft bg-warning-soft px-4 py-3">
          <div className="flex items-center gap-2">
            <RefreshCw className="h-4 w-4 text-warning animate-spin flex-shrink-0" />
            <p className="text-sm text-warning font-medium">Backup in progress</p>
          </div>
          <p className="mt-3 text-xs text-warning">
            Your files stay available the whole time. A large folder can take a
            while the first time it is copied — nothing is wrong, and it will
            not be cut short for taking long.
          </p>
        </div>
      )}

      {!opState.backing_up &&
        opState.last_ok_age_hours !== null &&
        opState.last_ok_age_hours >= opState.stale_after_hours && (
          <div className="rounded-lg border border-danger-soft bg-danger-soft px-4 py-3 flex items-start gap-2">
            <AlertTriangle className="h-4 w-4 text-danger flex-shrink-0 mt-0.5" />
            <div className="text-sm text-danger">
              <p className="font-medium">
                No backup has completed in{" "}
                {opState.last_ok_age_hours >= 48
                  ? `${Math.floor(opState.last_ok_age_hours / 24)} days`
                  : `${opState.last_ok_age_hours} hours`}
                .
              </p>
              <p className="mt-1">
                Anything you have changed since then is not saved anywhere else
                yet. Try Back Up Now.
              </p>
            </div>
          </div>
        )}

      {!opBusy &&
        opState.last_backup &&
        opState.last_backup.state === "crashed" && (
          <div className="rounded-lg border border-danger-soft bg-danger-soft px-4 py-3 flex items-start gap-2">
            <AlertTriangle className="h-4 w-4 text-danger flex-shrink-0 mt-0.5" />
            <div className="text-sm text-danger">
              <p className="font-medium">
                The last backup did not finish
                {opState.last_backup.error
                  ? `: ${opState.last_backup.error}`
                  : "."}{" "}
                Your previous backups are still safe — try running a new backup.
              </p>
            </div>
          </div>
        )}

      {loading ? (
        <div className="space-y-3">
          <Card>
            <CardContent className="pt-5 pb-5">
              <Shimmer className="h-14 w-full" />
            </CardContent>
          </Card>
          <Card>
            <CardContent className="pt-5 pb-5">
              <Shimmer className="h-14 w-full" />
            </CardContent>
          </Card>
        </div>
      ) : !s3Status?.provisioned ? (
        <EnableCard onEnable={handleEnable} disabled={opBusy} />
      ) : (
        <div className="space-y-3">
          {sets !== null && sets.length === 0 && !opState.backing_up && (
            <Card className="border-border">
              <CardContent className="pt-5 pb-5">
                <p className="text-sm text-fg-subtle">
                  No backups yet. Click{" "}
                  <span className="text-fg-muted">Backup Now</span> to create
                  the first one.
                </p>
              </CardContent>
            </Card>
          )}
          {sets?.map((s) => (
            <BackupSetCard
              key={s.id}
              set={s}
              runningNamespaces={runningNamespaces}
              disabled={opBusy}
              onRestoreStarted={() => void pollOpState()}
            />
          ))}
        </div>
      )}
    </div>
  );
}
