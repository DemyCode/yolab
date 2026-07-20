import { useEffect, useRef, useState, useCallback } from "react";
import {
  Database, RefreshCw, CheckCircle, AlertCircle,
  AlertTriangle, ChevronDown, ChevronRight, RotateCcw,
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

interface DrRestoreItem {
  namespace: string;
  pvc: string;
  result: string;
}

interface DrStatusResponse {
  restores: DrRestoreItem[];
  total: number;
  done: number;
  failed: number;
  all_complete: boolean;
}

interface OperationState {
  backing_up: boolean;
  restoring: boolean;
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

// ── Restore flow ──────────────────────────────────────────────────────────────

type RestoreStep = "diff" | "restoring" | "applying" | "done";

function RestoreFlow({
  snapshot,
  catalog,
  runningNamespaces,
  onCancel,
}: {
  snapshot: ResticSnapshot;
  catalog: SnapshotCatalog;
  runningNamespaces: Set<string>;
  onCancel: () => void;
}) {
  const [step, setStep] = useState<RestoreStep>("diff");
  const [selected, setSelected] = useState<Set<string>>(
    () => new Set(catalog.namespaces)
  );
  const [restoreItems, setRestoreItems] = useState<DrRestoreItem[]>([]);
  const [done, setDone] = useState(0);
  const [total, setTotal] = useState(0);
  const [failed, setFailed] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const pollRef = useRef<number | null>(null);

  useEffect(() => () => { if (pollRef.current) clearInterval(pollRef.current); }, []);

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
      next.has(ns) ? next.delete(ns) : next.add(ns);
      return next;
    });
  }

  function startPolling() {
    if (pollRef.current) return;
    pollRef.current = window.setInterval(async () => {
      try {
        const [s, state] = await Promise.all([
          fetch("/api/backups/dr/status").then(r => r.json()) as Promise<DrStatusResponse>,
          fetch("/api/backups/state").then(r => r.json()) as Promise<{ restoring: boolean }>,
        ]);
        setRestoreItems(s.restores);
        setDone(s.done);
        setTotal(s.total);
        setFailed(s.failed);
        // state.restoring is the backend's own read of whether any restore objects still
        // exist — prefer it over all_complete, which reads as permanently false (stuck at
        // "0/?") if this poll starts after the restore's ReplicationDestinations were
        // already cleaned up by the time we look.
        if (s.all_complete || !state.restoring) {
          clearInterval(pollRef.current!);
          pollRef.current = null;
          setStep("applying");
          await fetch("/api/backups/dr/apply", { method: "POST" });
          setStep("done");
        }
      } catch { /* network blip */ }
    }, 5000);
  }

  async function handleAccept() {
    setError(null);
    setStep("restoring");
    try {
      const res = await fetch("/api/backups/restore/from-snapshot", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ snapshot_id: snapshot.id, namespaces: [...selected] }),
      });
      if (!res.ok) throw new Error(await res.text());
      const data = await res.json() as { started: string[]; errors: string[] };
      if (data.errors.length && !data.started.length) {
        throw new Error(data.errors.join(", "));
      }
      setTotal(data.started.length);
      startPolling();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
      setStep("diff");
    }
  }

  // ── diff view ──
  if (step === "diff") {
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
            className="h-8 px-3 text-xs border-[#3f3f46] text-[#71717a] hover:text-[#fafafa]"
          >
            Cancel
          </Button>
          <Button
            onClick={handleAccept}
            disabled={selected.size === 0}
            className="h-8 px-4 text-xs bg-[#dc2626] hover:bg-[#b91c1c] text-white border-0 font-medium disabled:opacity-40"
          >
            Accept & Restore ({selected.size})
          </Button>
        </div>
      </div>
    );
  }

  // ── restoring / applying ──
  if (step === "restoring" || step === "applying") {
    return (
      <div className="border border-[#78350f] rounded-lg p-4 space-y-3 bg-[#1a1000]">
        <p className="text-sm font-semibold text-[#fbbf24] flex items-center gap-2">
          <RefreshCw className="h-4 w-4 animate-spin" />
          {step === "applying"
            ? "Starting services on restored data…"
            : `Pulling data from cloud… ${done}/${total || "?"}`}
        </p>
        {restoreItems.length > 0 && (
          <div className="space-y-1.5">
            {restoreItems.map(r => (
              <div key={`${r.namespace}/${r.pvc}`} className="flex items-center gap-2 text-xs">
                {r.result.toLowerCase() === "successful"
                  ? <CheckCircle className="h-3.5 w-3.5 text-[#4ade80] flex-shrink-0" />
                  : r.result.toLowerCase() === "failed"
                  ? <AlertCircle className="h-3.5 w-3.5 text-[#f87171] flex-shrink-0" />
                  : <RefreshCw className="h-3.5 w-3.5 text-[#fbbf24] animate-spin flex-shrink-0" />}
                <span className="text-[#fafafa]">{serviceNameFromNamespace(r.namespace)}</span>
                <span className="text-[#52525b] font-mono text-[11px]">{r.pvc}</span>
              </div>
            ))}
          </div>
        )}
        {failed > 0 && <p className="text-xs text-[#f87171]">{failed} failed — check VolSync logs.</p>}
      </div>
    );
  }

  // ── done ──
  return (
    <div className="border border-[#14532d] rounded-lg p-4 flex items-center justify-between gap-4 bg-[#0f1f0f]">
      <p className="text-sm font-semibold text-[#4ade80] flex items-center gap-2">
        <CheckCircle className="h-4 w-4" />
        Restore complete.
      </p>
      <Button
        onClick={onCancel}
        variant="outline"
        className="h-8 px-3 text-xs border-[#14532d] text-[#4ade80] hover:bg-[#14532d]"
      >
        Done
      </Button>
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
}: {
  snapshot: ResticSnapshot;
  runningNamespaces: Set<string>;
  isRestoring: boolean;
  disabled: boolean;
  onRestoreStart: () => void;
  onRestoreEnd: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [catalog, setCatalog] = useState<SnapshotCatalog | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [restoring, setRestoring] = useState(false);

  async function expand() {
    if (catalog) { setExpanded(e => !e); return; }
    setExpanded(true);
    setLoading(true);
    setError(null);
    try {
      const data = await fetch(`/api/backups/snapshots/${snapshot.id}/catalog`).then(r => r.json()) as SnapshotCatalog;
      setCatalog(data);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load");
    } finally {
      setLoading(false);
    }
  }

  function handleRestoreClick() {
    if (!catalog) return;
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
              disabled={!catalog || loading || disabled}
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
}: {
  runningNamespaces: Set<string>;
  onBackupDone: () => void;
  disabled: boolean;
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

  async function handleBackupNow() {
    setBackingUp(true);
    setBackupError(null);
    try {
      const res = await fetch("/api/backups/cluster/run-now", { method: "POST" });
      if (!res.ok) throw new Error(await res.text());
      await load();
      onBackupDone();
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
          {backingUp
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
          />
        ))
      )}
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

function DrBanner({ lostCount, disabled }: { lostCount: number; disabled: boolean }) {
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handle() {
    setStarting(true);
    setError(null);
    try {
      const res = await fetch("/api/backups/dr/start", { method: "POST" });
      if (!res.ok) throw new Error(await res.text());
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
    } finally { setStarting(false); }
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
  const [opState, setOpState]           = useState<OperationState>({ backing_up: false, restoring: false });

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
  useEffect(() => {
    let cancelled = false;
    async function poll() {
      try {
        const s = await fetch("/api/backups/state").then(r => r.json()) as OperationState;
        if (!cancelled) setOpState(s);
      } catch { /* network blip */ }
    }
    void poll();
    const id = window.setInterval(poll, 5000);
    return () => { cancelled = true; clearInterval(id); };
  }, []);

  const opBusy = opState.backing_up || opState.restoring;

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok) throw new Error(await res.text() || `Server error ${res.status}`);
    await load();
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Backups</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Each backup is a full snapshot of the cluster — K8s state, service configs, and all PVC data — encrypted and stored in Backblaze B2.
        </p>
      </div>

      {opBusy && (
        <div className="rounded-lg border border-[#78350f] bg-[#1a1000] px-4 py-3 flex items-center gap-2">
          <RefreshCw className="h-4 w-4 text-[#fbbf24] animate-spin flex-shrink-0" />
          <p className="text-sm text-[#fbbf24] font-medium">
            {opState.backing_up
              ? "Backup in progress — other backup actions are disabled until it finishes."
              : "Restore in progress — other backup actions are disabled until it finishes."}
          </p>
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
          {lostCount > 0 && <DrBanner lostCount={lostCount} disabled={opBusy} />}
          <SnapshotExplorer
            runningNamespaces={runningNamespaces}
            onBackupDone={load}
            disabled={opBusy}
          />
        </div>
      )}
    </div>
  );
}
