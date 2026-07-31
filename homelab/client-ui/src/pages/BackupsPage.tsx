import { useEffect, useRef, useState, useCallback } from "react";
import {
  Database, RefreshCw, CheckCircle, AlertCircle, Circle,
  AlertTriangle, ChevronDown, ChevronRight, RotateCcw, KeyRound, Copy,
} from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";

// ── Types ─────────────────────────────────────────────────────────────────────

interface PvcEntry {
  name: string;
  capacity: string;
}

interface ServiceEntry {
  namespace: string;
  pvcs: PvcEntry[];
}

interface SnapshotCatalog {
  timestamp: string;
  namespaces: string[];
  services?: ServiceEntry[];
}

interface ResticSnapshot {
  id: string;
  short_id: string;
  time: string;
}

interface DiffEntry {
  namespace: string;
  serviceName: string;
  pvcs: PvcEntry[];
  mode: "adding" | "recovering";
}

// Mirrors backup_run.rs's BackupRun.status shape.
interface BackupRunStatus {
  phase: "Pending" | "SyncingVolumes" | "SnapshottingCluster" | "Pruning" | "Succeeded" | "Partial" | "Failed";
  startedAt?: string;
  finishedAt?: string;
  error?: string | null;
  stalePvcs?: string[];
  snapshotId?: string;
}

// Mirrors restore_run.rs's RestoreRun.status shape.
interface VolumeStatus {
  pvc: string;
  phase: "Pending" | "Restoring" | "Succeeded" | "Failed" | "Skipped";
}

interface NamespaceRestoreStatus {
  namespace: string;
  scaledDeployments: string[];
  volumes: VolumeStatus[];
}

interface RestoreRunStatus {
  phase: "Validating" | "WaitingForStorage" | "RestoringVolumes" | "Applying" | "Succeeded" | "Partial" | "Failed";
  startedAt?: string;
  finishedAt?: string;
  error?: string | null;
  snapshotId?: string;
  restoreAsOf?: string | null;
  namespaces?: NamespaceRestoreStatus[];
}

interface DrStatusResponse {
  active: RestoreRunStatus | null;
  last: RestoreRunStatus | null;
}

interface OperationState {
  backing_up: boolean;
  restoring: boolean;
  backup_run: BackupRunStatus | null;
  restore_run: RestoreRunStatus | null;
  last_backup: BackupRunStatus | null;
}

interface RecoveryKeyResponse {
  configured: boolean;
  recovery_key?: string;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function Shimmer({ className }: { className?: string }) {
  return <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />;
}

function serviceNameFromNamespace(ns: string): string {
  const s = ns.replace(/^yolab-/, "");
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function formatDate(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    year: "numeric", month: "short", day: "numeric",
    hour: "2-digit", minute: "2-digit",
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
//
// A restore touches live, mounted data: apps get scaled to 0, PVCs get deleted and
// recreated, ReplicationDestinations pull from B2. Letting the user start a second
// backup/restore or navigate the rest of this page mid-flight would race against
// that. So instead of an inline card, this replaces the ENTIRE backups page for as
// long as a RestoreRun is non-terminal — the same full-page treatment as, say, an
// OS installer, since the operation is just as disruptive and just as important to
// watch to completion (or at least to a safe terminal state).

const RESTORE_PHASES: { key: RestoreRunStatus["phase"]; label: string }[] = [
  { key: "Validating", label: "Validating snapshot" },
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
      return <CheckCircle className="h-4 w-4 text-[#4ade80] flex-shrink-0" />;
    case "Failed":
      return <AlertCircle className="h-4 w-4 text-[#f87171] flex-shrink-0" />;
    case "Skipped":
      return <AlertTriangle className="h-4 w-4 text-[#fbbf24] flex-shrink-0" />;
    case "Restoring":
      return <RefreshCw className="h-4 w-4 text-[#a78bfa] animate-spin flex-shrink-0" />;
    default:
      return <Circle className="h-4 w-4 text-[#3f3f46] flex-shrink-0" />;
  }
}

function volumePhaseLabel(phase: VolumeStatus["phase"]): string {
  switch (phase) {
    case "Succeeded": return "Restored";
    case "Failed": return "Failed";
    case "Skipped": return "No backup found — kept as-is";
    case "Restoring": return "Restoring…";
    default: return "Pending";
  }
}

function RestoreTakeover({ onDone }: { onDone: () => void }) {
  const [status, setStatus] = useState<RestoreRunStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    async function poll() {
      try {
        const data = await fetch("/api/backups/dr/status").then(r => r.json()) as DrStatusResponse;
        if (cancelled) return;
        setStatus(data.active ?? data.last ?? null);
      } catch { /* network blip */ }
    }
    void poll();
    const id = window.setInterval(poll, 3000);
    return () => { cancelled = true; clearInterval(id); };
  }, []);

  if (!status) {
    return (
      <div className="flex flex-col items-center justify-center min-h-[60vh] gap-3">
        <RefreshCw className="h-6 w-6 text-[#a78bfa] animate-spin" />
        <p className="text-sm text-[#a1a1aa]">Starting restore…</p>
      </div>
    );
  }

  const terminal = isTerminalRestorePhase(status.phase);
  const currentIndex = RESTORE_PHASES.findIndex(p => p.key === status.phase);
  const namespaces = status.namespaces ?? [];
  const totalVolumes = namespaces.reduce((n, ns) => n + ns.volumes.length, 0);
  const succeededVolumes = namespaces.reduce(
    (n, ns) => n + ns.volumes.filter(v => v.phase === "Succeeded").length, 0
  );

  return (
    <div className="min-h-[70vh] flex flex-col max-w-3xl mx-auto w-full">
      <div className="mb-8">
        <h1 className="text-xl font-semibold text-[#fafafa]">Restoring from backup</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          {status.snapshotId ? `Snapshot ${status.snapshotId.slice(0, 8)}` : "Restore in progress"}
          {status.restoreAsOf ? ` · as of ${formatDate(status.restoreAsOf)}` : ""}
          {" — "}other backup and restore actions are disabled until this finishes.
        </p>
      </div>

      {/* Phase stepper */}
      {!terminal && (
        <div className="flex items-center mb-8">
          {RESTORE_PHASES.map((p, i) => (
            <div key={p.key} className="flex items-center flex-1 last:flex-none">
              <div className="flex flex-col items-center gap-2">
                <div className={`h-8 w-8 rounded-full flex items-center justify-center border-2 ${
                  i < currentIndex ? "border-[#4ade80] bg-[#4ade80]/10" :
                  i === currentIndex ? "border-[#a78bfa] bg-[#a78bfa]/10" :
                  "border-[#3f3f46]"
                }`}>
                  {i < currentIndex ? (
                    <CheckCircle className="h-4 w-4 text-[#4ade80]" />
                  ) : i === currentIndex ? (
                    <RefreshCw className="h-4 w-4 text-[#a78bfa] animate-spin" />
                  ) : (
                    <span className="text-xs text-[#52525b]">{i + 1}</span>
                  )}
                </div>
                <span className={`text-xs whitespace-nowrap ${i === currentIndex ? "text-[#fafafa] font-medium" : "text-[#52525b]"}`}>
                  {p.label}
                </span>
              </div>
              {i < RESTORE_PHASES.length - 1 && (
                <div className={`flex-1 h-px mx-2 ${i < currentIndex ? "bg-[#4ade80]" : "bg-[#3f3f46]"}`} />
              )}
            </div>
          ))}
        </div>
      )}

      {/* Terminal banner */}
      {terminal && (
        <div className={`rounded-lg border px-4 py-3 mb-8 flex items-start gap-2 ${
          status.phase === "Succeeded" ? "border-[#14532d] bg-[#0f1f0f]" :
          status.phase === "Partial" ? "border-[#78350f] bg-[#1a1000]" :
          "border-[#7f1d1d] bg-[#1a0000]"
        }`}>
          {status.phase === "Succeeded" ? (
            <CheckCircle className="h-4 w-4 text-[#4ade80] flex-shrink-0 mt-0.5" />
          ) : (
            <AlertTriangle className={`h-4 w-4 flex-shrink-0 mt-0.5 ${status.phase === "Partial" ? "text-[#fbbf24]" : "text-[#f87171]"}`} />
          )}
          <div className="text-sm">
            <p className={`font-medium ${
              status.phase === "Succeeded" ? "text-[#4ade80]" :
              status.phase === "Partial" ? "text-[#fbbf24]" : "text-[#f87171]"
            }`}>
              {status.phase === "Succeeded" && `Restore complete — ${succeededVolumes}/${totalVolumes || 0} volume${totalVolumes === 1 ? "" : "s"} restored.`}
              {status.phase === "Partial" && `Restore finished with issues — ${succeededVolumes}/${totalVolumes} volumes restored. Affected services are running with their previous or empty data.`}
              {status.phase === "Failed" && `Restore failed${status.error ? `: ${status.error}` : "."}`}
            </p>
          </div>
        </div>
      )}

      {/* Per-namespace / per-volume progress */}
      {namespaces.length > 0 && (
        <div className="space-y-3 flex-1">
          {namespaces.map(ns => (
            <Card key={ns.namespace} className="border-[#27272a]">
              <CardContent className="pt-4 pb-4">
                <p className="text-sm font-medium text-[#fafafa] mb-2">{serviceNameFromNamespace(ns.namespace)}</p>
                {ns.volumes.length === 0 ? (
                  <p className="text-xs text-[#52525b]">No volumes — configuration restored only.</p>
                ) : (
                  <div className="space-y-1.5">
                    {ns.volumes.map(v => (
                      <div key={v.pvc} className="flex items-center gap-2 text-xs">
                        <VolumePhaseIcon phase={v.phase} />
                        <span className="text-[#a1a1aa] font-mono">{v.pvc}</span>
                        <span className="text-[#52525b] ml-auto">{volumePhaseLabel(v.phase)}</span>
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
            className="h-9 px-4 text-sm bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium"
          >
            Back to Backups
          </Button>
        </div>
      )}
    </div>
  );
}

// ── Restore flow (confirm step) ───────────────────────────────────────────────
//
// Once accepted, the RestoreRun takes over the whole page (see RestoreTakeover
// above) — this component's job ends at kicking the restore off.

function RestoreFlow({
  snapshot,
  catalog,
  runningNamespaces,
  onCancel,
  onStarted,
}: {
  snapshot: ResticSnapshot;
  catalog: SnapshotCatalog;
  runningNamespaces: Set<string>;
  onCancel: () => void;
  onStarted: () => void;
}) {
  const [selected, setSelected] = useState<Set<string>>(
    () => new Set(catalog.namespaces)
  );
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const services: ServiceEntry[] = catalog.services ??
    catalog.namespaces.map(ns => ({ namespace: ns, pvcs: [] }));

  const diff: DiffEntry[] = services.map(svc => ({
    namespace: svc.namespace,
    serviceName: serviceNameFromNamespace(svc.namespace),
    pvcs: svc.pvcs,
    mode: runningNamespaces.has(svc.namespace) ? "recovering" : "adding",
  }));

  const addingCount     = diff.filter(e => e.mode === "adding"     && selected.has(e.namespace)).length;
  const recoveringCount = diff.filter(e => e.mode === "recovering" && selected.has(e.namespace)).length;

  function toggle(ns: string) {
    setSelected(prev => {
      const next = new Set(prev);
      if (next.has(ns)) { next.delete(ns); } else { next.add(ns); }
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
        body: JSON.stringify({ snapshot_id: snapshot.id, namespaces: [...selected] }),
      });
      if (!res.ok) throw new Error(await res.text());
      onStarted();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
      setStarting(false);
    }
  }

  return (
    <div className="border border-[#3f3f46] rounded-lg p-4 space-y-4 bg-[#0f0f11]">
      <div className="flex items-center justify-between gap-2">
        <p className="text-sm font-semibold text-[#fafafa]">
          Restore from {formatDate(snapshot.time)}
        </p>
        <button onClick={onCancel} className="text-xs text-[#52525b] hover:text-[#a1a1aa]">✕ Cancel</button>
      </div>

      <div className="space-y-2">
        {diff.map(entry => (
          <label key={entry.namespace} className="flex items-start gap-3 cursor-pointer">
            <input
              type="checkbox"
              checked={selected.has(entry.namespace)}
              onChange={() => toggle(entry.namespace)}
              className="mt-0.5 h-4 w-4 rounded border-[#3f3f46] bg-[#18181b] accent-[#a78bfa]"
            />
            <div className="flex-1 min-w-0">
              <div className="flex items-center gap-2">
                <span className="text-sm text-[#fafafa] font-medium">{entry.serviceName}</span>
                {entry.mode === "adding" ? (
                  <span className="text-xs text-[#4ade80] font-medium">Adding</span>
                ) : (
                  <span className="text-xs text-[#fbbf24] font-medium">Recovering</span>
                )}
              </div>
              {entry.pvcs.length > 0 && (
                <p className="text-xs text-[#52525b] mt-0.5">
                  {entry.pvcs.map(p => `${p.name} (${p.capacity})`).join(" · ")}
                </p>
              )}
            </div>
          </label>
        ))}
      </div>

      {selected.size > 0 && (
        <div className="rounded border border-[#7f1d1d] bg-[#1c0a0a] px-3 py-2 text-xs text-[#fca5a5] space-y-0.5">
          <p className="font-medium flex items-center gap-1.5">
            <AlertTriangle className="h-3.5 w-3.5 flex-shrink-0" />
            This cannot be undone — current data will be replaced.
          </p>
          {addingCount > 0 && <p>· {addingCount} service{addingCount !== 1 ? "s" : ""} will be created from backup.</p>}
          {recoveringCount > 0 && <p>· {recoveringCount} running service{recoveringCount !== 1 ? "s" : ""} will be stopped and restored.</p>}
        </div>
      )}

      {error && <p className="text-xs text-[#f87171]">{error}</p>}

      <div className="flex justify-end gap-2">
        <Button
          variant="outline"
          onClick={onCancel}
          disabled={starting}
          className="h-8 px-3 text-xs border-[#3f3f46] text-[#71717a] hover:text-[#fafafa]"
        >
          Cancel
        </Button>
        <Button
          onClick={handleAccept}
          disabled={selected.size === 0 || starting}
          className="h-8 px-4 text-xs bg-[#dc2626] hover:bg-[#b91c1c] text-white border-0 font-medium disabled:opacity-40"
        >
          {starting ? <><RefreshCw className="h-3 w-3 mr-1.5 animate-spin" />Starting…</> : `Accept & Restore (${selected.size})`}
        </Button>
      </div>
    </div>
  );
}

// ── Snapshot card ─────────────────────────────────────────────────────────────

function SnapshotCard({
  snapshot,
  runningNamespaces,
  isRestoring,
  disabled,
  onRestoreStart,
  onRestoreEnd,
  onRestoreStarted,
}: {
  snapshot: ResticSnapshot;
  runningNamespaces: Set<string>;
  isRestoring: boolean;
  disabled: boolean;
  onRestoreStart: () => void;
  onRestoreEnd: () => void;
  onRestoreStarted: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [catalog, setCatalog] = useState<SnapshotCatalog | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [restoring, setRestoring] = useState(false);

  // Returns the loaded catalog, fetching it first if this card hasn't been expanded yet.
  // Used by both the row-expand toggle and "Restore from here" — the latter is visible (and
  // was previously clickable-but-silently-a-no-op) before the row's ever been expanded, since
  // it doesn't live inside the expanded section.
  async function ensureCatalogLoaded(): Promise<SnapshotCatalog | null> {
    if (catalog) return catalog;
    setLoading(true);
    setError(null);
    try {
      const data = await fetch(`/api/backups/snapshots/${snapshot.id}/catalog`).then(r => r.json()) as SnapshotCatalog;
      setCatalog(data);
      return data;
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load");
      return null;
    } finally {
      setLoading(false);
    }
  }

  async function expand() {
    if (catalog) { setExpanded(e => !e); return; }
    setExpanded(true);
    await ensureCatalogLoaded();
  }

  async function handleRestoreClick() {
    setExpanded(true);
    const data = await ensureCatalogLoaded();
    if (!data) return; // fetch failed — `error` is already shown in the expanded section
    setRestoring(true);
    onRestoreStart();
  }

  function handleRestoreEnd() {
    setRestoring(false);
    setExpanded(false);
    onRestoreEnd();
  }

  const serviceCount = catalog ? (catalog.services?.length ?? catalog.namespaces.length) : null;

  return (
    <Card className="border-[#27272a]">
      <CardContent className="pt-4 pb-4">
        {/* Header row */}
        <div className="flex items-center gap-3">
          <button
            onClick={expand}
            className="flex items-center gap-2 flex-1 min-w-0 text-left"
          >
            {expanded
              ? <ChevronDown className="h-4 w-4 text-[#52525b] flex-shrink-0" />
              : <ChevronRight className="h-4 w-4 text-[#52525b] flex-shrink-0" />}
            <div className="flex-1 min-w-0">
              <span className="text-sm font-medium text-[#fafafa]">{formatDate(snapshot.time)}</span>
              <span className="ml-2 text-xs text-[#52525b]">{timeAgo(snapshot.time)}</span>
              {serviceCount !== null && (
                <span className="ml-2 text-xs text-[#71717a]">
                  · {serviceCount} service{serviceCount !== 1 ? "s" : ""}
                </span>
              )}
            </div>
          </button>
          {!restoring && !isRestoring && (
            <Button
              onClick={handleRestoreClick}
              disabled={loading || disabled}
              variant="outline"
              className="flex-shrink-0 h-7 px-3 text-xs border-[#3f3f46] text-[#a78bfa] hover:border-[#a78bfa] hover:text-[#a78bfa] disabled:opacity-30"
            >
              {loading ? <RefreshCw className="h-3 w-3 animate-spin" /> : "Restore from here"}
            </Button>
          )}
        </div>

        {/* Expanded content */}
        {expanded && (
          <div className="mt-3 pl-6 space-y-3">
            {loading && <Shimmer className="h-12 w-full" />}
            {error && <p className="text-xs text-[#f87171]">{error}</p>}

            {catalog && !restoring && (
              <div className="space-y-2">
                {(catalog.services ?? catalog.namespaces.map(ns => ({ namespace: ns, pvcs: [] }))).map(svc => (
                  <div key={svc.namespace} className="flex items-start gap-3">
                    <Database className="h-3.5 w-3.5 text-[#52525b] mt-0.5 flex-shrink-0" />
                    <div>
                      <span className="text-sm text-[#a1a1aa]">
                        {serviceNameFromNamespace(svc.namespace)}
                      </span>
                      {svc.pvcs.length > 0 && (
                        <span className="ml-2 text-xs text-[#52525b]">
                          {svc.pvcs.map(p => `${p.name} ${p.capacity}`).join(" · ")}
                        </span>
                      )}
                      <span className="ml-2 text-xs">
                        {runningNamespaces.has(svc.namespace) ? (
                          <span className="text-[#fbbf24]">Recovering</span>
                        ) : (
                          <span className="text-[#4ade80]">Adding</span>
                        )}
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            )}

            {restoring && catalog && (
              <RestoreFlow
                snapshot={snapshot}
                catalog={catalog}
                runningNamespaces={runningNamespaces}
                onCancel={handleRestoreEnd}
                onStarted={onRestoreStarted}
              />
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

// ── Snapshot explorer ─────────────────────────────────────────────────────────

function SnapshotExplorer({
  runningNamespaces,
  onBackupDone,
  disabled,
  backupInProgress,
  onRestoreStarted,
}: {
  runningNamespaces: Set<string>;
  onBackupDone: () => void;
  disabled: boolean;
  backupInProgress: boolean;
  onRestoreStarted: () => void;
}) {
  const [snapshots, setSnapshots] = useState<ResticSnapshot[] | null>(null);
  const [backingUp, setBackingUp] = useState(false);
  const [backupError, setBackupError] = useState<string | null>(null);
  const [activeRestore, setActiveRestore] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const res = await fetch("/api/backups/snapshots").then(r => r.json()) as { snapshots: ResticSnapshot[] };
      const sorted = (res.snapshots ?? []).sort(
        (a, b) => new Date(b.time).getTime() - new Date(a.time).getTime()
      );
      setSnapshots(sorted);
    } catch {
      setSnapshots([]);
    }
  }, []);

  useEffect(() => { void load(); }, [load]);

  // The backup runs in the background on the server (it outlives the HTTP request).
  // When the global backup-in-progress flag flips from true → false, the new
  // snapshot exists — reload the list so it appears without a manual refresh.
  const prevBackingUp = useRef(false);
  useEffect(() => {
    if (prevBackingUp.current && !backupInProgress) {
      void load();
      onBackupDone();
    }
    prevBackingUp.current = backupInProgress;
  }, [backupInProgress, load, onBackupDone]);

  async function handleBackupNow() {
    setBackingUp(true);
    setBackupError(null);
    try {
      const res = await fetch("/api/backups/cluster/run-now", { method: "POST" });
      if (!res.ok) throw new Error(await res.text());
      // Backup now runs detached on the server and survives this request ending.
      // Progress is tracked by the global backup-state poll (the "Backup in progress"
      // banner); the effect above refreshes the snapshot list when it completes.
    } catch (e) {
      setBackupError(e instanceof Error ? e.message : "Backup failed");
    } finally {
      setBackingUp(false);
    }
  }

  return (
    <div className="space-y-3">
      {/* Header + Backup Now */}
      <div className="flex items-center justify-between gap-3">
        <div>
          <p className="text-sm font-semibold text-[#fafafa]">Backup Snapshots</p>
          <p className="text-xs text-[#52525b] mt-0.5">
            Each snapshot is a full picture of the cluster at that moment — K8s state + PVC data.
          </p>
        </div>
        <Button
          onClick={handleBackupNow}
          disabled={backingUp || disabled}
          variant="outline"
          className="flex-shrink-0 h-8 px-3 text-xs border-[#3f3f46] text-[#a1a1aa] hover:text-[#fafafa] disabled:opacity-40"
        >
          {backingUp || backupInProgress
            ? <><RefreshCw className="h-3 w-3 mr-1.5 animate-spin" />Backing up…</>
            : <><RotateCcw className="h-3 w-3 mr-1.5" />Backup Now</>}
        </Button>
      </div>

      {backupError && <p className="text-xs text-[#f87171]">{backupError}</p>}

      {/* Snapshot list */}
      {snapshots === null ? (
        <div className="space-y-2">
          <Card><CardContent className="pt-4 pb-4"><Shimmer className="h-8 w-full" /></CardContent></Card>
          <Card><CardContent className="pt-4 pb-4"><Shimmer className="h-8 w-full" /></CardContent></Card>
        </div>
      ) : snapshots.length === 0 ? (
        <Card className="border-[#27272a]">
          <CardContent className="pt-5 pb-5">
            <p className="text-sm text-[#52525b]">
              No snapshots yet. Click <span className="text-[#a1a1aa]">Backup Now</span> to create the first one.
            </p>
          </CardContent>
        </Card>
      ) : (
        snapshots.map(snap => (
          <SnapshotCard
            key={snap.id}
            snapshot={snap}
            runningNamespaces={runningNamespaces}
            isRestoring={activeRestore !== null && activeRestore !== snap.id}
            disabled={disabled}
            onRestoreStart={() => setActiveRestore(snap.id)}
            onRestoreEnd={() => { setActiveRestore(null); void load(); }}
            onRestoreStarted={onRestoreStarted}
          />
        ))
      )}
    </div>
  );
}

// ── Recovery key overlay ──────────────────────────────────────────────────────
//
// The restic encryption password is generated locally and is never sent to
// yolab-external — that's what guarantees yolab-external can never read your data,
// even with full account access. The tradeoff: this key is the ONLY way to decrypt
// your B2 backups if this machine is lost, so it must be shown to the user and
// explicitly saved somewhere durable (password manager, printed copy, etc).
//
// `mandatory` controls whether it can be dismissed without acknowledging — true
// right after enabling backups (first and most important viewing), false when
// reopened later via "View recovery key" (already presumably saved once).
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
    } catch { /* clipboard unavailable — user can still select-and-copy */ }
  }

  return (
    <div className="fixed inset-0 z-50 bg-[#09090b]/95 backdrop-blur-sm flex items-center justify-center p-6">
      <div className="max-w-lg w-full border border-[#3f3f46] rounded-lg bg-[#0f0f11] p-6 space-y-4">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0 bg-[#2d2a1a]">
            <KeyRound className="h-4 w-4 text-[#fbbf24]" strokeWidth={1.75} />
          </div>
          <div>
            <p className="text-sm font-semibold text-[#fafafa]">Your backup recovery key</p>
            <p className="text-xs text-[#a1a1aa] mt-1">
              This is the only way to decrypt your backups if this machine is lost or destroyed.
              YoLab does not store a copy anywhere else. Save it now in a password manager or
              print it — without it, your backups on Backblaze B2 are permanently unreadable.
            </p>
          </div>
        </div>

        <div className="flex items-center gap-2">
          <code className="flex-1 text-sm font-mono text-[#fafafa] bg-[#18181b] border border-[#3f3f46] rounded px-3 py-2 break-all select-all">
            {recoveryKey}
          </code>
          <Button
            onClick={handleCopy}
            variant="outline"
            className="flex-shrink-0 h-9 px-3 text-xs border-[#3f3f46] text-[#a1a1aa] hover:text-[#fafafa]"
          >
            {copied ? <CheckCircle className="h-3.5 w-3.5 text-[#4ade80]" /> : <Copy className="h-3.5 w-3.5" />}
          </Button>
        </div>

        {mandatory && (
          <label className="flex items-start gap-2 cursor-pointer">
            <input
              type="checkbox"
              checked={acknowledged}
              onChange={() => setAcknowledged(a => !a)}
              className="mt-0.5 h-4 w-4 rounded border-[#3f3f46] bg-[#18181b] accent-[#a78bfa]"
            />
            <span className="text-xs text-[#a1a1aa]">
              I've saved this recovery key somewhere safe and durable.
            </span>
          </label>
        )}

        <div className="flex justify-end">
          <Button
            onClick={onClose}
            disabled={mandatory && !acknowledged}
            className="h-8 px-4 text-xs bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium disabled:opacity-40"
          >
            {mandatory ? "I've saved it — continue" : "Close"}
          </Button>
        </div>
      </div>
    </div>
  );
}

// ── Enable card ───────────────────────────────────────────────────────────────

function EnableCard({ onEnable, disabled }: { onEnable: () => Promise<void>; disabled: boolean }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handle() {
    setBusy(true);
    setError(null);
    try { await onEnable(); }
    catch (e) { setError(e instanceof Error ? e.message : "Failed"); }
    finally { setBusy(false); }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0 bg-[#2d2a1a]">
            <Database className="h-4 w-4 text-[#fbbf24]" strokeWidth={1.75} />
          </div>
          <div className="flex-1">
            <div className="flex items-center justify-between gap-4 flex-wrap">
              <div>
                <p className="text-sm font-medium text-[#fafafa]">Backups not configured</p>
                <p className="text-xs text-[#71717a] mt-0.5">
                  Enable to start daily encrypted backups to Backblaze B2
                </p>
              </div>
              <Button
                onClick={handle}
                disabled={busy || disabled}
                className="bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium text-sm h-8 px-3 disabled:opacity-40"
              >
                {busy
                  ? <><RefreshCw className="h-3.5 w-3.5 mr-1.5 animate-spin" />Enabling…</>
                  : "Enable Backups"}
              </Button>
            </div>
            {error && <p className="mt-2 text-xs text-[#f87171]">{error}</p>}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── DR banner (auto-detected Lost PVCs) ───────────────────────────────────────

function DrBanner({ lostCount, disabled, onRestoreStarted }: { lostCount: number; disabled: boolean; onRestoreStarted: () => void }) {
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handle() {
    setStarting(true);
    setError(null);
    try {
      const res = await fetch("/api/backups/dr/start", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        // Emergency restore = every managed namespace, from the latest snapshot
        // (omitting snapshot_id resolves to the latest cluster-backup snapshot).
        body: JSON.stringify({ all: true }),
      });
      if (!res.ok) throw new Error(await res.text());
      onRestoreStarted();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
      setStarting(false);
    }
  }

  return (
    <div className="rounded-lg border border-[#7f1d1d] bg-[#1c0a0a] p-4 space-y-2">
      <div className="flex items-start justify-between gap-4 flex-wrap">
        <div>
          <p className="text-sm font-semibold text-[#f87171] flex items-center gap-2">
            <AlertTriangle className="h-4 w-4" />
            Disk failure detected
          </p>
          <p className="text-xs text-[#71717a] mt-1">
            {lostCount} volume{lostCount !== 1 ? "s" : ""} lost. Restore all data from the last backup.
          </p>
        </div>
        <Button
          onClick={handle}
          disabled={starting || disabled}
          className="flex-shrink-0 bg-[#dc2626] hover:bg-[#b91c1c] text-white border-0 text-sm h-9 px-4 disabled:opacity-40"
        >
          {starting
            ? <><RefreshCw className="h-3.5 w-3.5 mr-1.5 animate-spin" />Starting…</>
            : "Emergency Restore"}
        </Button>
      </div>
      {error && <p className="text-xs text-[#f87171]">{error}</p>}
    </div>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

export function BackupsPage() {
  const [s3Status, setS3Status]         = useState<{ provisioned: boolean } | null>(null);
  const [runningNamespaces, setRunning] = useState<Set<string>>(new Set());
  const [lostCount, setLostCount]       = useState(0);
  const [loading, setLoading]           = useState(true);
  const [opState, setOpState]           = useState<OperationState>({
    backing_up: false, restoring: false, backup_run: null, restore_run: null, last_backup: null,
  });
  const [recoveryKey, setRecoveryKey]   = useState<string | null>(null);
  const [recoveryMandatory, setRecoveryMandatory] = useState(false);

  async function showRecoveryKey(mandatory: boolean) {
    try {
      const data = await fetch("/api/backups/recovery-key").then(r => r.json()) as RecoveryKeyResponse;
      if (data.configured && data.recovery_key) {
        setRecoveryKey(data.recovery_key);
        setRecoveryMandatory(mandatory);
      }
    } catch { /* network blip — user can retry via "View recovery key" */ }
  }

  const load = useCallback(async () => {
    const [s3Res, statusRes] = await Promise.all([
      fetch("/api/backups/s3").then(r => r.json()).catch(() => ({ provisioned: false })),
      fetch("/api/backups/status").then(r => r.json()).catch(() => null),
    ]);
    setS3Status(s3Res as { provisioned: boolean });

    const status = statusRes as { pvcs?: { namespace: string; pvc_phase?: string }[] } | null;
    if (status?.pvcs) {
      setRunning(new Set(status.pvcs.map(p => p.namespace)));
      setLostCount(status.pvcs.filter(p => p.pvc_phase === "Lost" || p.pvc_phase === "NotFound").length);
    }
    setLoading(false);
  }, []);

  useEffect(() => { void load(); }, [load]);

  // Single source of truth for "is a backup or restore currently running" — read from the
  // backend on a timer, never tracked locally, so a page refresh or a second tab can't
  // desync from what's actually happening.
  const pollOpState = useCallback(async () => {
    try {
      const s = await fetch("/api/backups/state").then(r => r.json()) as OperationState;
      setOpState(s);
      return s;
    } catch {
      return null; // network blip
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    const id = window.setInterval(() => { if (!cancelled) void pollOpState(); }, 5000);
    void pollOpState();
    return () => { cancelled = true; clearInterval(id); };
  }, [pollOpState]);

  const opBusy = opState.backing_up || opState.restoring;

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok) throw new Error(await res.text() || `Server error ${res.status}`);
    await load();
    // First and most important viewing — the key was just generated, and this
    // is the only moment the user is guaranteed to still be in the setup flow.
    await showRecoveryKey(true);
  }

  // A RestoreRun is disruptive enough (deployments scaled to 0, PVCs deleted and
  // recreated) that it takes over the entire page — see RestoreTakeover's doc comment.
  if (opState.restoring) {
    return (
      <RestoreTakeover
        onDone={() => {
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
          <h1 className="text-xl font-semibold text-[#fafafa]">Backups</h1>
          <p className="text-sm text-[#71717a] mt-0.5">
            Each backup is a full snapshot of the cluster — K8s state, service configs, and all PVC data — encrypted and stored in Backblaze B2.
          </p>
        </div>
        {s3Status?.provisioned && (
          <button
            onClick={() => void showRecoveryKey(false)}
            className="flex-shrink-0 flex items-center gap-1.5 text-xs text-[#71717a] hover:text-[#fafafa]"
          >
            <KeyRound className="h-3.5 w-3.5" />
            View recovery key
          </button>
        )}
      </div>

      {opState.backing_up && (
        <div className="rounded-lg border border-[#78350f] bg-[#1a1000] px-4 py-3 flex items-center gap-2">
          <RefreshCw className="h-4 w-4 text-[#fbbf24] animate-spin flex-shrink-0" />
          <p className="text-sm text-[#fbbf24] font-medium">
            Backup in progress
            {opState.backup_run ? ` (${opState.backup_run.phase})` : ""}
            {" "}— other backup actions are disabled until it finishes.
          </p>
        </div>
      )}

      {!opBusy && opState.last_backup && opState.last_backup.phase !== "Succeeded" && (
        <div className="rounded-lg border border-[#7f1d1d] bg-[#1a0000] px-4 py-3 flex items-start gap-2">
          <AlertTriangle className="h-4 w-4 text-[#f87171] flex-shrink-0 mt-0.5" />
          <div className="text-sm text-[#f87171]">
            {opState.last_backup.phase === "Failed" ? (
              <p className="font-medium">
                The last backup failed{opState.last_backup.error ? `: ${opState.last_backup.error}` : "."} Your previous
                backups are still safe — try running a new backup.
              </p>
            ) : (
              <>
                <p className="font-medium">
                  The last backup completed, but some volumes could not be backed up in time and kept their previous
                  snapshot:
                </p>
                <ul className="mt-1 list-disc list-inside text-[#fca5a5]">
                  {(opState.last_backup.stalePvcs ?? []).map((p) => <li key={p}>{p}</li>)}
                </ul>
                <p className="mt-1 text-[#fca5a5]">Run another backup once the cluster is idle to capture their latest data.</p>
              </>
            )}
          </div>
        </div>
      )}

      {loading ? (
        <div className="space-y-3">
          <Card><CardContent className="pt-5 pb-5"><Shimmer className="h-14 w-full" /></CardContent></Card>
          <Card><CardContent className="pt-5 pb-5"><Shimmer className="h-14 w-full" /></CardContent></Card>
        </div>
      ) : !s3Status?.provisioned ? (
        <EnableCard onEnable={handleEnable} disabled={opBusy} />
      ) : (
        <div className="space-y-4">
          {lostCount > 0 && (
            <DrBanner
              lostCount={lostCount}
              disabled={opBusy}
              onRestoreStarted={() => void pollOpState()}
            />
          )}
          <SnapshotExplorer
            runningNamespaces={runningNamespaces}
            onBackupDone={load}
            disabled={opBusy}
            backupInProgress={opState.backing_up}
            onRestoreStarted={() => void pollOpState()}
          />
        </div>
      )}
    </div>
  );
}
