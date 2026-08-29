import { useEffect, useRef, useState, useCallback } from "react";
import {
  Database,
  RefreshCw,
  CheckCircle,
  AlertCircle,
  Circle,
  AlertTriangle,
  ChevronDown,
  ChevronRight,
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
  /// Catalog app id (e.g. "gitea"), captured at backup time from the namespace's
  /// yolab.io/app-id annotation. Absent on snapshots taken before identity was exported.
  app_id?: string;
  instance_name?: string;
  /// Exact images the namespace was running — with the catalog digest-pinned, this
  /// identifies the version the data belongs to.
  images?: string[];
}

interface SnapshotCatalog {
  timestamp: string;
  namespaces: string[];
  services?: ServiceEntry[];
  /// Repo commit the node was built from when this backup was taken.
  catalog_version?: string | null;
}

interface ResticSnapshot {
  id: string;
  short_id: string;
  time: string;
}

interface DiffEntry {
  namespace: string;
  serviceName: string;
  appId?: string;
  pvcs: PvcEntry[];
  mode: "adding" | "recovering";
}

// Mirrors backup_run.rs's BackupRun.status shape.
interface BackupRunStatus {
  phase:
    | "Pending"
    | "SyncingVolumes"
    | "SnapshottingCluster"
    | "Pruning"
    | "Succeeded"
    | "Partial"
    | "Failed";
  startedAt?: string;
  finishedAt?: string;
  error?: string | null;
  stalePvcs?: string[];
  snapshotId?: string;
  /// Whether the etcd (cluster state) half of the backup actually made it in — a run
  /// can otherwise succeed on volumes alone with cluster state silently missing.
  etcdIncluded?: boolean;
  /// Per-volume progress. The server has always sent this — flatten_status passes the
  /// whole CR status through — but it was never declared here and never rendered, so
  /// the page reduced eight volumes with individual states to the single word
  /// "SyncingVolumes" and left no way to tell which one was holding a run up.
  pvcs?: BackupVolumeStatus[];
}

/// One volume inside a running backup. `percent`/`eta` come from restic's own progress
/// output and are absent until it emits its first line, so both are optional and the
/// row simply shows less rather than showing a zero that means "unknown".
interface BackupVolumeStatus {
  namespace: string;
  name: string;
  phase: "Syncing" | "Synced" | "Stalled";
  percent?: number;
  eta?: string;
}

// Mirrors restore_run.rs's RestoreRun.status shape.
interface VolumeStatus {
  pvc: string;
  /// Deleting: the old PVC has been asked to go away and we're waiting it out across
  /// reconcile ticks (nothing blocks server-side), after which the restore target is
  /// recreated and the data pulled back.
  phase:
    | "Pending"
    | "Deleting"
    | "Restoring"
    | "Succeeded"
    | "Failed"
    | "Skipped";
}

interface DeploymentScale {
  name: string;
  replicas: number;
}

interface NamespaceRestoreStatus {
  namespace: string;
  /// Recorded before scaling to zero, so the original replica count is restored rather
  /// than everything being flattened to 1.
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
  /// Set when the run hit a timeout or hard error and was routed through recovery
  /// (scaling apps back up) instead of stopping where it was. Present means the final
  /// phase is Partial/Failed even if individual volumes succeeded.
  abortReason?: string | null;
  /// Repo commit the restored data was backed up from.
  restoredFromVersion?: string | null;
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
  /// Hours since the last backup that produced a restorable snapshot (Partial counts —
  /// seven of eight volumes captured is still seven volumes you can restore). Null when
  /// none ever has. This, not how long a run takes, is the number that says whether
  /// backups are working.
  last_ok_age_hours: number | null;
  /// The age at which the server considers that stale, so this page states the rule
  /// rather than hardcoding a threshold beside it.
  stale_after_hours: number;
}

interface RecoveryKeyResponse {
  configured: boolean;
  recovery_key?: string;
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
  { key: "RebuildingStorage", label: "Rebuilding storage" },
  { key: "WaitingForStorage", label: "Waiting for storage" },
  { key: "RestoringVolumes", label: "Restoring volumes" },
  { key: "Applying", label: "Bringing services back up" },
];

function isTerminalRestorePhase(phase: string): boolean {
  return phase === "Succeeded" || phase === "Partial" || phase === "Failed";
}

// ── Schedule ──────────────────────────────────────────────────────────────────

interface SchedulePreview {
  valid: boolean;
  expr: string;
  description?: string;
  timezone?: string;
  next?: string | null;
  next_local?: string | null;
  error?: string;
  configured?: boolean;
  default?: string;
}

/// The everyday answers, so nobody has to know cron to change when backups run. The
/// field stays visible and editable underneath — a preset is a shortcut, not a cage.
const SCHEDULE_PRESETS: { label: string; expr: string }[] = [
  { label: "Every 6 hours", expr: "0 */6 * * *" },
  { label: "Every day", expr: "0 2 * * *" },
  { label: "Every week", expr: "0 3 * * 0" },
  { label: "Every month", expr: "0 4 1 * *" },
];

/// When backups run.
///
/// The plain-English line under the field is produced by the SERVER, from the same
/// parser that decides when backups actually start. Describing cron in the browser
/// would be quicker to type and able to drift from the scheduler — confidently telling
/// someone "every day at 02:00" while the thing running their backups reads it
/// differently is the one failure this screen must not have.
function ScheduleCard() {
  const [expr, setExpr] = useState("");
  const [saved, setSaved] = useState<string | null>(null);
  const [preview, setPreview] = useState<SchedulePreview | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch("/api/backups/schedule")
      .then((r) => r.json())
      .then((d: SchedulePreview) => {
        if (cancelled) return;
        setExpr(d.expr);
        setSaved(d.expr);
        setPreview(d);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  // Debounced so typing does not fire a request per keystroke, and late replies for
  // an expression that is no longer in the box are dropped rather than rendered.
  useEffect(() => {
    if (!expr.trim()) {
      setPreview(null);
      return;
    }
    let cancelled = false;
    const t = setTimeout(() => {
      fetch(`/api/backups/schedule/preview?expr=${encodeURIComponent(expr)}`)
        .then((r) => r.json())
        .then((d: SchedulePreview) => {
          if (!cancelled) setPreview(d);
        })
        .catch(() => {});
    }, 250);
    return () => {
      cancelled = true;
      clearTimeout(t);
    };
  }, [expr]);

  const dirty = saved !== null && expr.trim() !== saved;

  async function save() {
    setSaving(true);
    setSaveError(null);
    try {
      const res = await fetch("/api/backups/schedule", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ expr }),
      });
      const d: SchedulePreview = await res.json();
      if (!res.ok) throw new Error(d.error ?? `Server error ${res.status}`);
      // Render what the server stored, not what was typed — they differ whenever the
      // expression was normalised.
      setExpr(d.expr);
      setSaved(d.expr);
      setPreview(d);
    } catch (e) {
      setSaveError(
        e instanceof Error ? e.message : "Could not save the schedule",
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <Card>
      <CardContent className="p-4 space-y-3">
        <div>
          <h3 className="text-sm font-medium text-fg">When backups run</h3>
          <p className="text-xs text-fg-muted mt-0.5">
            If the machine is asleep at the scheduled time, the backup runs as
            soon as it wakes up — it is not skipped.
          </p>
        </div>

        <div className="flex flex-wrap gap-2">
          {SCHEDULE_PRESETS.map((p) => (
            <button
              key={p.expr}
              type="button"
              onClick={() => setExpr(p.expr)}
              className={`rounded-md border px-2.5 py-1 text-xs transition-colors ${
                expr.trim() === p.expr
                  ? "border-primary bg-primary-soft text-primary font-medium"
                  : "border-border text-fg-muted hover:text-fg hover:border-border-strong"
              }`}
            >
              {p.label}
            </button>
          ))}
        </div>

        <div>
          <input
            value={expr}
            onChange={(e) => setExpr(e.target.value)}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            aria-label="Backup schedule, as a cron expression"
            aria-invalid={preview ? !preview.valid : undefined}
            className={`w-full rounded-md border bg-bg px-3 py-2 font-mono text-sm text-fg outline-none focus:ring-1 ${
              preview && !preview.valid
                ? "border-danger focus:ring-danger"
                : "border-border focus:ring-primary focus:border-primary"
            }`}
            placeholder="0 2 * * *"
          />

          {/* The translation. Never absent while there is something in the box: a
              cron field with no readable meaning beneath it is what made this
              setting unusable in the first place. */}
          <div className="mt-2 min-h-[2.5rem] text-xs">
            {preview === null ? (
              <span className="text-fg-muted">Checking…</span>
            ) : preview.valid ? (
              <div className="space-y-0.5">
                <p className="text-fg">
                  <span className="text-fg-muted">Runs </span>
                  <span className="font-medium">{preview.description}</span>
                  {preview.timezone && (
                    <span className="text-fg-muted"> ({preview.timezone})</span>
                  )}
                </p>
                {preview.next_local && (
                  <p className="text-fg-muted">
                    Next backup: {preview.next_local}
                  </p>
                )}
              </div>
            ) : (
              <p className="text-danger">{preview.error}</p>
            )}
          </div>
        </div>

        {saveError && <p className="text-xs text-danger">{saveError}</p>}

        <div className="flex items-center gap-2">
          <Button
            size="sm"
            onClick={save}
            disabled={!dirty || saving || (preview ? !preview.valid : true)}
          >
            {saving ? "Saving…" : "Save schedule"}
          </Button>
          {dirty && !saving && (
            <button
              type="button"
              onClick={() => saved !== null && setExpr(saved)}
              className="text-xs text-fg-muted hover:text-fg"
            >
              Cancel
            </button>
          )}
          {!dirty && preview?.configured === false && (
            <span className="text-xs text-fg-muted">
              Using the default schedule.
            </span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

/// What a backup is doing, in words, with the counts the phase name hides.
///
/// The page used to print the raw Rust constant — "Backup in progress
/// (SyncingVolumes)" — which names an implementation phase rather than telling anyone
/// what is happening to their files or how far along it is.
function backupPhaseLabel(run: BackupRunStatus): string {
  const vols = run.pvcs ?? [];
  const done = vols.filter((v) => v.phase === "Synced").length;
  switch (run.phase) {
    case "Pending":
      return "Getting ready…";
    case "SyncingVolumes":
      return vols.length > 0
        ? `Copying your files — ${done} of ${vols.length} done`
        : "Copying your files…";
    case "SnapshottingCluster":
      return "Saving your apps' settings…";
    case "Pruning":
      return "Tidying up old backups…";
    default:
      return run.phase;
  }
}

/// The volume rows behind that summary: which are done, which is copying, and how far.
///
/// Everything here was already in the response and already on the page's own props —
/// it just had nowhere to be drawn. A stalled volume is called out rather than hidden,
/// because it is the one case where the run finishes without that volume in it.
function BackupVolumeList({ volumes }: { volumes: BackupVolumeStatus[] }) {
  if (volumes.length === 0) return null;
  // Whatever is still moving goes first — that is what someone is looking for.
  const order = { Stalled: 0, Syncing: 1, Synced: 2 } as const;
  const sorted = [...volumes].sort(
    (a, b) => order[a.phase] - order[b.phase] || a.name.localeCompare(b.name),
  );
  return (
    <ul className="mt-3 space-y-1.5">
      {sorted.map((v) => (
        <li
          key={`${v.namespace}/${v.name}`}
          className="flex items-center gap-2 text-xs"
        >
          {v.phase === "Synced" ? (
            <CheckCircle className="h-3.5 w-3.5 text-success flex-shrink-0" />
          ) : v.phase === "Stalled" ? (
            <AlertTriangle className="h-3.5 w-3.5 text-warning flex-shrink-0" />
          ) : (
            <RefreshCw className="h-3.5 w-3.5 text-primary animate-spin flex-shrink-0" />
          )}
          <span className="text-fg truncate">{v.name}</span>
          <span className="text-fg-muted ml-auto flex-shrink-0 tabular-nums">
            {v.phase === "Synced"
              ? "Backed up"
              : v.phase === "Stalled"
                ? "Stopped responding"
                : v.percent !== undefined
                  ? `${v.percent.toFixed(0)}%${v.eta ? ` · ${v.eta} left` : ""}`
                  : "Copying…"}
          </span>
        </li>
      ))}
    </ul>
  );
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

      {/* Phase stepper */}
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

      {/* Terminal banner */}
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
            {/* A run that timed out or hit a hard error still runs recovery before
                finishing, so the apps are back up — say so explicitly rather than
                leaving the user wondering whether anything is still running. */}
            {status.abortReason && (
              <p className="text-xs text-fg-muted mt-1">
                {status.abortReason} — services were scaled back up
                automatically.
              </p>
            )}
          </div>
        </div>
      )}

      {/* Per-namespace / per-volume progress */}
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
    () => new Set(catalog.namespaces),
  );
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Asked here rather than passed down: this dialog is the only place the answer
  // changes what happens, and it must be the state NOW, not whenever the page loaded
  // — a disk can be reconnected between opening Backups and confirming a restore.
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
    // Only worth showing when the instance was named something other than the app it
    // came from ("myfiles" running filebrowser) — otherwise it just repeats the title.
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
          snapshot_id: snapshot.id,
          namespaces: [...selected],
          // When storage is damaged beyond repair, restoring into it is impossible —
          // it has to be recreated first. That used to be four Ceph commands run by
          // hand, which is not a recovery anyone here can perform. The restore does
          // it, and the server re-checks against the cluster before deleting
          // anything.
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
          Restore from {formatDate(snapshot.time)}
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

      {/* Said before the button, not after: when storage is beyond repair this
          restore does more than restore, and the extra part is not reversible. */}
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
      const data = (await fetch(
        `/api/backups/snapshots/${snapshot.id}/catalog`,
      ).then((r) => r.json())) as SnapshotCatalog;
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
    if (catalog) {
      setExpanded((e) => !e);
      return;
    }
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

  const serviceCount = catalog
    ? (catalog.services?.length ?? catalog.namespaces.length)
    : null;

  return (
    <Card className="border-border">
      <CardContent className="pt-4 pb-4">
        {/* Header row */}
        <div className="flex items-center gap-3">
          <button
            onClick={expand}
            className="flex items-center gap-2 flex-1 min-w-0 text-left"
          >
            {expanded ? (
              <ChevronDown className="h-4 w-4 text-fg-subtle flex-shrink-0" />
            ) : (
              <ChevronRight className="h-4 w-4 text-fg-subtle flex-shrink-0" />
            )}
            <div className="flex-1 min-w-0">
              <span className="text-sm font-medium text-fg">
                {formatDate(snapshot.time)}
              </span>
              <span className="ml-2 text-xs text-fg-subtle">
                {timeAgo(snapshot.time)}
              </span>
              {serviceCount !== null && (
                <span className="ml-2 text-xs text-fg-muted">
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
              className="flex-shrink-0 h-7 px-3 text-xs border-border-strong text-primary hover:border-primary hover:text-primary disabled:opacity-30"
            >
              {loading ? (
                <RefreshCw className="h-3 w-3 animate-spin" />
              ) : (
                "Restore from here"
              )}
            </Button>
          )}
        </div>

        {/* Expanded content */}
        {expanded && (
          <div className="mt-3 pl-6 space-y-3">
            {loading && <Shimmer className="h-12 w-full" />}
            {error && <p className="text-xs text-danger">{error}</p>}

            {catalog && !restoring && (
              <div className="space-y-2">
                {(
                  catalog.services ??
                  catalog.namespaces.map((ns) => ({ namespace: ns, pvcs: [] }))
                ).map((svc) => (
                  <div key={svc.namespace} className="flex items-start gap-3">
                    <Database className="h-3.5 w-3.5 text-fg-subtle mt-0.5 flex-shrink-0" />
                    <div>
                      <span className="text-sm text-fg-muted">
                        {serviceNameFromNamespace(svc.namespace)}
                      </span>
                      {svc.pvcs.length > 0 && (
                        <span className="ml-2 text-xs text-fg-subtle">
                          {svc.pvcs
                            .map((p) => `${p.name} ${p.capacity}`)
                            .join(" · ")}
                        </span>
                      )}
                      <span className="ml-2 text-xs">
                        {runningNamespaces.has(svc.namespace) ? (
                          <span className="text-warning">Recovering</span>
                        ) : (
                          <span className="text-success">Adding</span>
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
      const res = (await fetch("/api/backups/snapshots").then((r) =>
        r.json(),
      )) as { snapshots: ResticSnapshot[] };
      const sorted = (res.snapshots ?? []).sort(
        (a, b) => new Date(b.time).getTime() - new Date(a.time).getTime(),
      );
      setSnapshots(sorted);
    } catch {
      setSnapshots([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

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
      const res = await fetch("/api/backups/cluster/run-now", {
        method: "POST",
      });
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
          <p className="text-sm font-semibold text-fg">Backup Snapshots</p>
          <p className="text-xs text-fg-subtle mt-0.5">
            Each snapshot is a full picture of the cluster at that moment — K8s
            state + PVC data.
          </p>
        </div>
        <Button
          onClick={handleBackupNow}
          disabled={backingUp || disabled}
          variant="outline"
          className="flex-shrink-0 h-8 px-3 text-xs border-border-strong text-fg-muted hover:text-fg disabled:opacity-40"
        >
          {backingUp || backupInProgress ? (
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
      </div>

      {backupError && <p className="text-xs text-danger">{backupError}</p>}

      {/* Snapshot list */}
      {snapshots === null ? (
        <div className="space-y-2">
          <Card>
            <CardContent className="pt-4 pb-4">
              <Shimmer className="h-8 w-full" />
            </CardContent>
          </Card>
          <Card>
            <CardContent className="pt-4 pb-4">
              <Shimmer className="h-8 w-full" />
            </CardContent>
          </Card>
        </div>
      ) : snapshots.length === 0 ? (
        <Card className="border-border">
          <CardContent className="pt-5 pb-5">
            <p className="text-sm text-fg-subtle">
              No snapshots yet. Click{" "}
              <span className="text-fg-muted">Backup Now</span> to create the
              first one.
            </p>
          </CardContent>
        </Card>
      ) : (
        snapshots.map((snap) => (
          <SnapshotCard
            key={snap.id}
            snapshot={snap}
            runningNamespaces={runningNamespaces}
            isRestoring={activeRestore !== null && activeRestore !== snap.id}
            disabled={disabled}
            onRestoreStart={() => setActiveRestore(snap.id)}
            onRestoreEnd={() => {
              setActiveRestore(null);
              void load();
            }}
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
  const [loading, setLoading] = useState(true);
  const [opState, setOpState] = useState<OperationState>({
    backing_up: false,
    restoring: false,
    backup_run: null,
    restore_run: null,
    last_backup: null,
    // null means "we have not heard yet", which must not render as "0 hours ago"
    // and claim a fresh backup exists before the first poll returns.
    last_ok_age_hours: null,
    stale_after_hours: 24,
  });
  const [recoveryKey, setRecoveryKey] = useState<string | null>(null);
  const [recoveryMandatory, setRecoveryMandatory] = useState(false);
  // Separate from opState.restoring: that flips false the instant the RestoreRun reaches
  // a terminal phase, which would unmount the takeover before its Succeeded/Partial/Failed
  // banner ever rendered. This stays true until the user explicitly dismisses the takeover
  // (its "Back to Backups" button), so the terminal result is always seen.
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
    const [s3Res, statusRes] = await Promise.all([
      fetch("/api/backups/s3")
        .then((r) => r.json())
        .catch(() => ({ provisioned: false })),
      fetch("/api/backups/status")
        .then((r) => r.json())
        .catch(() => null),
    ]);
    setS3Status(s3Res as { provisioned: boolean });

    const status = statusRes as {
      pvcs?: { namespace: string; pvc_phase?: string }[];
    } | null;
    if (status?.pvcs) {
      setRunning(new Set(status.pvcs.map((p) => p.namespace)));
    }
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // Single source of truth for "is a backup or restore currently running" — read from the
  // backend on a timer, never tracked locally, so a page refresh or a second tab can't
  // desync from what's actually happening.
  const pollOpState = useCallback(async () => {
    try {
      const s = (await fetch("/api/backups/state").then((r) =>
        r.json(),
      )) as OperationState;
      setOpState(s);
      return s;
    } catch {
      return null; // network blip
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

  // Latch onto the takeover as soon as a restore is observed running — whether it just
  // started (from this tab's own action) or was already in progress on page load.
  useEffect(() => {
    if (opState.restoring) setShowRestoreView(true);
  }, [opState.restoring]);

  const opBusy = opState.backing_up || opState.restoring;

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok)
      throw new Error((await res.text()) || `Server error ${res.status}`);
    await load();
    // First and most important viewing — the key was just generated, and this
    // is the only moment the user is guaranteed to still be in the setup flow.
    await showRecoveryKey(true);
  }

  // A RestoreRun is disruptive enough (deployments scaled to 0, PVCs deleted and
  // recreated) that it takes over the entire page — see RestoreTakeover's doc comment.
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
        {s3Status?.provisioned && (
          <button
            onClick={() => void showRecoveryKey(false)}
            className="flex-shrink-0 flex items-center gap-1.5 text-xs text-fg-muted hover:text-fg"
          >
            <KeyRound className="h-3.5 w-3.5" />
            View recovery key
          </button>
        )}
      </div>

      {opState.backing_up && (
        <div className="rounded-lg border border-warning-soft bg-warning-soft px-4 py-3">
          <div className="flex items-center gap-2">
            <RefreshCw className="h-4 w-4 text-warning animate-spin flex-shrink-0" />
            <p className="text-sm text-warning font-medium">
              {opState.backup_run
                ? backupPhaseLabel(opState.backup_run)
                : "Backup in progress"}
            </p>
          </div>
          {opState.backup_run?.pvcs && (
            <BackupVolumeList volumes={opState.backup_run.pvcs} />
          )}
          <p className="mt-3 text-xs text-warning">
            Your files stay available the whole time. A large folder can take a
            while the first time it is copied — nothing is wrong, and it will
            not be cut short for taking long.
          </p>
        </div>
      )}

      {/* Staleness, not duration. A backup that is merely slow is fine; one that
          has not completed in days is the actual failure, and it is invisible
          unless the page says so. */}
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
                yet. Try Back Up Now, and if it keeps failing the message above
                will say which part is stuck.
              </p>
            </div>
          </div>
        )}

      {!opBusy &&
        opState.last_backup &&
        opState.last_backup.phase !== "Succeeded" && (
          <div className="rounded-lg border border-danger-soft bg-danger-soft px-4 py-3 flex items-start gap-2">
            <AlertTriangle className="h-4 w-4 text-danger flex-shrink-0 mt-0.5" />
            <div className="text-sm text-danger">
              {opState.last_backup.phase === "Failed" ? (
                <p className="font-medium">
                  The last backup failed
                  {opState.last_backup.error
                    ? `: ${opState.last_backup.error}`
                    : "."}{" "}
                  Your previous backups are still safe — try running a new
                  backup.
                </p>
              ) : (
                <>
                  <p className="font-medium">
                    The last backup completed, but some volumes could not be
                    backed up in time and kept their previous snapshot:
                  </p>
                  <ul className="mt-1 list-disc list-inside text-danger">
                    {(opState.last_backup.stalePvcs ?? []).map((p) => (
                      <li key={p}>{p}</li>
                    ))}
                  </ul>
                  <p className="mt-1 text-danger">
                    Run another backup once the cluster is idle to capture their
                    latest data.
                  </p>
                </>
              )}
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
        <div className="space-y-4">
          {/* Above the snapshot list: when backups run is a setting, and the list
              below it is the result of that setting. */}
          <ScheduleCard />
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
