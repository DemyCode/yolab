import { useEffect, useRef, useState, useCallback } from "react";
import {
  Database, RefreshCw, CheckCircle, AlertCircle, Clock,
  AlertTriangle, RotateCcw, ChevronDown,
} from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";

// ── Types ─────────────────────────────────────────────────────────────────────

interface PvcStatus {
  namespace: string;
  pvc: string;
  last_sync_time: string | null;
  last_sync_duration: string | null;
  result: string;
  pvc_phase?: string;
}

interface BackupStatus {
  pvcs: PvcStatus[];
  etcd_last_snapshot: string | null;
  dr_mode?: boolean;
}

interface ResticSnapshot {
  id: string;
  short_id: string;
  time: string;
}

interface DiffEntry {
  namespace: string;
  serviceName: string;
  mode: "adding" | "recovering";
}

type RestoreFlow = "idle" | "loading-snapshots" | "picking" | "loading-catalog" | "diff" | "confirming" | "restoring" | "applying" | "done";

interface DrRestoreItem {
  namespace: string;
  pvc: string;
  result: string;
  last_sync_time: string | null;
}

interface DrStatusResponse {
  restores: DrRestoreItem[];
  total: number;
  done: number;
  failed: number;
  all_complete: boolean;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function Shimmer({ className }: { className?: string }) {
  return <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />;
}

function timeAgo(iso: string | null): string {
  if (!iso) return "never";
  const diff = Date.now() - new Date(iso).getTime();
  const h = Math.floor(diff / 3600000);
  const m = Math.floor((diff % 3600000) / 60000);
  if (h > 48) return `${Math.floor(h / 24)}d ago`;
  if (h > 0) return `${h}h ${m}m ago`;
  return `${m}m ago`;
}

function formatSnapshotDate(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleString(undefined, {
    year: "numeric", month: "short", day: "numeric",
    hour: "2-digit", minute: "2-digit",
  });
}

function serviceNameFromNamespace(ns: string): string {
  const s = ns.replace(/^yolab-/, "");
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function ResultBadge({ result }: { result: string }) {
  const lower = result.toLowerCase();
  if (lower === "successful" || lower === "true") {
    return (
      <span className="flex items-center gap-1 text-[#4ade80] text-xs font-medium">
        <CheckCircle className="h-3.5 w-3.5" /> Synced
      </span>
    );
  }
  if (lower === "failed" || lower === "false") {
    return (
      <span className="flex items-center gap-1 text-[#f87171] text-xs font-medium">
        <AlertCircle className="h-3.5 w-3.5" /> Failed
      </span>
    );
  }
  return (
    <span className="flex items-center gap-1 text-[#fbbf24] text-xs font-medium">
      <Clock className="h-3.5 w-3.5" /> {result || "Pending"}
    </span>
  );
}

// ── Restore Panel ─────────────────────────────────────────────────────────────

function RestorePanel({ currentNamespaces }: { currentNamespaces: Set<string> }) {
  const [flow, setFlow] = useState<RestoreFlow>("idle");
  const [snapshots, setSnapshots] = useState<ResticSnapshot[]>([]);
  const [selectedSnapshotId, setSelectedSnapshotId] = useState<string>("");
  const [diff, setDiff] = useState<DiffEntry[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [restoreItems, setRestoreItems] = useState<DrRestoreItem[]>([]);
  const [restoreDone, setRestoreDone] = useState(0);
  const [restoreTotal, setRestoreTotal] = useState(0);
  const [restoreFailed, setRestoreFailed] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const pollRef = useRef<number | null>(null);

  useEffect(() => () => { if (pollRef.current) clearInterval(pollRef.current); }, []);

  async function openPicker() {
    setFlow("loading-snapshots");
    setError(null);
    try {
      const res = await fetch("/api/backups/snapshots").then(r => r.json()) as { snapshots: ResticSnapshot[]; configured: boolean };
      const snaps = (res.snapshots ?? []).sort(
        (a, b) => new Date(b.time).getTime() - new Date(a.time).getTime()
      );
      setSnapshots(snaps);
      setSelectedSnapshotId(snaps[0]?.id ?? "");
      setFlow("picking");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load snapshots");
      setFlow("idle");
    }
  }

  async function loadCatalog(snapshotId: string) {
    setFlow("loading-catalog");
    setError(null);
    try {
      const catalog = await fetch(`/api/backups/snapshots/${snapshotId}/catalog`).then(r => r.json()) as { namespaces?: string[] };
      const namespaces: string[] = catalog.namespaces ?? [];
      const entries: DiffEntry[] = namespaces.map(ns => ({
        namespace: ns,
        serviceName: serviceNameFromNamespace(ns),
        mode: currentNamespaces.has(ns) ? "recovering" : "adding",
      }));
      setDiff(entries);
      setSelected(new Set(namespaces));
      setFlow("diff");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load catalog");
      setFlow("picking");
    }
  }

  function toggleService(ns: string) {
    setSelected(prev => {
      const next = new Set(prev);
      if (next.has(ns)) next.delete(ns);
      else next.add(ns);
      return next;
    });
  }

  function startPolling() {
    if (pollRef.current) return;
    pollRef.current = window.setInterval(async () => {
      try {
        const s = await fetch("/api/backups/dr/status").then(r => r.json()) as DrStatusResponse;
        setRestoreItems(s.restores);
        setRestoreDone(s.done);
        setRestoreTotal(s.total);
        setRestoreFailed(s.failed);
        if (s.all_complete) {
          clearInterval(pollRef.current!);
          pollRef.current = null;
          setFlow("applying");
          const apply = await fetch("/api/backups/dr/apply", { method: "POST" });
          if (!apply.ok) setError(await apply.text());
          setFlow("done");
        }
      } catch {
        // network blip — keep polling
      }
    }, 5000);
  }

  async function handleAccept() {
    setError(null);
    setFlow("restoring");
    try {
      const res = await fetch("/api/backups/restore/from-snapshot", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          snapshot_id: selectedSnapshotId,
          namespaces: [...selected],
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      const data = await res.json() as { started: string[]; errors: string[] };
      if (data.started.length === 0) {
        setError(data.errors.join(", ") || "Nothing started — no PVCs found for selected services.");
        setFlow("diff");
        return;
      }
      setRestoreTotal(data.started.length);
      setRestoreDone(0);
      setRestoreFailed(0);
      setRestoreItems([]);
      startPolling();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to start restore");
      setFlow("diff");
    }
  }

  const selectedSnapshot = snapshots.find(s => s.id === selectedSnapshotId);

  // ── idle ──
  if (flow === "idle") {
    return (
      <Card className="border-[#3f3f46]">
        <CardContent className="pt-5 pb-5">
          <div className="flex items-center justify-between gap-4">
            <div>
              <p className="text-sm font-semibold text-[#fafafa] flex items-center gap-2">
                <RotateCcw className="h-4 w-4 text-[#a78bfa]" />
                Restore from Backup
              </p>
              <p className="text-xs text-[#71717a] mt-0.5">
                Roll back one or more services to a specific backup date.
              </p>
            </div>
            <Button
              onClick={openPicker}
              className="flex-shrink-0 bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium text-sm h-9 px-4"
            >
              Select Backup Date
            </Button>
          </div>
        </CardContent>
      </Card>
    );
  }

  // ── loading-snapshots ──
  if (flow === "loading-snapshots") {
    return (
      <Card className="border-[#3f3f46]">
        <CardContent className="pt-5 pb-5">
          <div className="flex items-center gap-2 text-sm text-[#a1a1aa]">
            <RefreshCw className="h-4 w-4 animate-spin" />
            Loading available backups…
          </div>
        </CardContent>
      </Card>
    );
  }

  // ── picking ──
  if (flow === "picking") {
    return (
      <Card className="border-[#3f3f46]">
        <CardContent className="pt-5 pb-5 space-y-4">
          <p className="text-sm font-semibold text-[#fafafa] flex items-center gap-2">
            <RotateCcw className="h-4 w-4 text-[#a78bfa]" />
            Select a backup date
          </p>

          {snapshots.length === 0 ? (
            <p className="text-xs text-[#71717a]">No cluster backups found. The first backup runs nightly at 02:00 UTC.</p>
          ) : (
            <div className="relative">
              <select
                value={selectedSnapshotId}
                onChange={e => setSelectedSnapshotId(e.target.value)}
                className="w-full appearance-none bg-[#18181b] border border-[#3f3f46] rounded-md text-sm text-[#fafafa] px-3 py-2 pr-8 focus:outline-none focus:border-[#a78bfa]"
              >
                {snapshots.map(s => (
                  <option key={s.id} value={s.id}>
                    {formatSnapshotDate(s.time)} — {s.short_id}
                  </option>
                ))}
              </select>
              <ChevronDown className="absolute right-2 top-1/2 -translate-y-1/2 h-4 w-4 text-[#71717a] pointer-events-none" />
            </div>
          )}

          {error && <p className="text-xs text-[#f87171]">{error}</p>}

          <div className="flex gap-2 justify-end">
            <Button
              variant="outline"
              className="h-8 px-3 text-xs border-[#3f3f46] text-[#71717a] hover:text-[#fafafa]"
              onClick={() => { setFlow("idle"); setError(null); }}
            >
              Cancel
            </Button>
            {snapshots.length > 0 && (
              <Button
                onClick={() => loadCatalog(selectedSnapshotId)}
                className="h-8 px-4 text-xs bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium"
              >
                Next →
              </Button>
            )}
          </div>
        </CardContent>
      </Card>
    );
  }

  // ── loading-catalog ──
  if (flow === "loading-catalog") {
    return (
      <Card className="border-[#3f3f46]">
        <CardContent className="pt-5 pb-5">
          <div className="flex items-center gap-2 text-sm text-[#a1a1aa]">
            <RefreshCw className="h-4 w-4 animate-spin" />
            Reading backup contents…
          </div>
        </CardContent>
      </Card>
    );
  }

  // ── diff ──
  if (flow === "diff") {
    const addingCount     = diff.filter(e => e.mode === "adding"     && selected.has(e.namespace)).length;
    const recoveringCount = diff.filter(e => e.mode === "recovering" && selected.has(e.namespace)).length;

    return (
      <Card className="border-[#3f3f46]">
        <CardContent className="pt-5 pb-5 space-y-4">
          <div>
            <p className="text-sm font-semibold text-[#fafafa] flex items-center gap-2">
              <RotateCcw className="h-4 w-4 text-[#a78bfa]" />
              Restore from {selectedSnapshot ? formatSnapshotDate(selectedSnapshot.time) : "backup"}
            </p>
            <p className="text-xs text-[#71717a] mt-0.5">
              Tick the services you want to restore.
            </p>
          </div>

          {diff.length === 0 ? (
            <p className="text-xs text-[#71717a]">No services found in this backup.</p>
          ) : (
            <div className="space-y-2">
              {diff.map(entry => (
                <label
                  key={entry.namespace}
                  className="flex items-center gap-3 cursor-pointer group"
                >
                  <input
                    type="checkbox"
                    checked={selected.has(entry.namespace)}
                    onChange={() => toggleService(entry.namespace)}
                    className="h-4 w-4 rounded border-[#3f3f46] bg-[#18181b] accent-[#a78bfa]"
                  />
                  <div className="flex-1 min-w-0">
                    <span className="text-sm text-[#fafafa]">{entry.serviceName}</span>
                    {entry.mode === "adding" ? (
                      <span className="ml-2 text-xs text-[#4ade80] font-medium">Adding</span>
                    ) : (
                      <span className="ml-2 text-xs text-[#fbbf24] font-medium">Recovering</span>
                    )}
                  </div>
                </label>
              ))}
            </div>
          )}

          {selected.size > 0 && (
            <div className="rounded-md border border-[#7f1d1d] bg-[#1c0a0a] p-3 text-xs text-[#fca5a5] space-y-1">
              <p className="font-medium flex items-center gap-1.5">
                <AlertTriangle className="h-3.5 w-3.5 flex-shrink-0" />
                This will replace current data — it cannot be undone.
              </p>
              {addingCount > 0 && (
                <p>· {addingCount} service{addingCount !== 1 ? "s" : ""} will be created and filled from backup.</p>
              )}
              {recoveringCount > 0 && (
                <p>· {recoveringCount} running service{recoveringCount !== 1 ? "s" : ""} will be stopped and their data replaced.</p>
              )}
            </div>
          )}

          {error && <p className="text-xs text-[#f87171]">{error}</p>}

          <div className="flex gap-2 justify-end">
            <Button
              variant="outline"
              className="h-8 px-3 text-xs border-[#3f3f46] text-[#71717a] hover:text-[#fafafa]"
              onClick={() => { setFlow("picking"); setError(null); }}
            >
              ← Back
            </Button>
            <Button
              onClick={handleAccept}
              disabled={selected.size === 0}
              className="h-8 px-4 text-xs bg-[#dc2626] hover:bg-[#b91c1c] text-white border-0 font-medium disabled:opacity-40"
            >
              Accept & Restore {selected.size > 0 ? `(${selected.size})` : ""}
            </Button>
          </div>
        </CardContent>
      </Card>
    );
  }

  // ── restoring ──
  if (flow === "restoring" || flow === "applying") {
    return (
      <Card className="border-[#78350f]" style={{ background: "#1a1000" }}>
        <CardContent className="pt-5 pb-5 space-y-3">
          <p className="text-sm font-semibold text-[#fbbf24] flex items-center gap-2">
            <RefreshCw className="h-4 w-4 animate-spin" />
            {flow === "applying"
              ? "Starting services on restored data…"
              : `Restoring… ${restoreDone}/${restoreTotal || "?"} complete`}
          </p>

          {restoreItems.length > 0 && (
            <div className="space-y-1.5">
              {restoreItems.map(r => (
                <div key={`${r.namespace}/${r.pvc}`} className="flex items-center gap-2 text-xs">
                  {r.result.toLowerCase() === "successful" ? (
                    <CheckCircle className="h-3.5 w-3.5 text-[#4ade80] flex-shrink-0" />
                  ) : r.result.toLowerCase() === "failed" ? (
                    <AlertCircle className="h-3.5 w-3.5 text-[#f87171] flex-shrink-0" />
                  ) : (
                    <RefreshCw className="h-3.5 w-3.5 text-[#fbbf24] animate-spin flex-shrink-0" />
                  )}
                  <span className="text-[#fafafa]">{serviceNameFromNamespace(r.namespace)}</span>
                  <span
                    className="text-xs"
                    style={{
                      color: r.result.toLowerCase() === "successful" ? "#4ade80"
                           : r.result.toLowerCase() === "failed"     ? "#f87171"
                           : "#fbbf24",
                    }}
                  >
                    {r.result.toLowerCase() === "successful" ? "Done"
                   : r.result.toLowerCase() === "failed"     ? "Failed"
                   : "Pulling from cloud…"}
                  </span>
                </div>
              ))}
            </div>
          )}

          {restoreFailed > 0 && (
            <p className="text-xs text-[#f87171]">
              {restoreFailed} restore{restoreFailed !== 1 ? "s" : ""} failed — check VolSync logs.
            </p>
          )}
          {error && <p className="text-xs text-[#f87171]">{error}</p>}
        </CardContent>
      </Card>
    );
  }

  // ── done ──
  if (flow === "done") {
    return (
      <Card className="border-[#14532d]" style={{ background: "#0f1f0f" }}>
        <CardContent className="pt-5 pb-5">
          <div className="flex items-center justify-between gap-4">
            <p className="text-sm font-semibold text-[#4ade80] flex items-center gap-2">
              <CheckCircle className="h-4 w-4" />
              Restore complete — all services are back up.
            </p>
            <Button
              variant="outline"
              className="h-8 px-3 text-xs border-[#14532d] text-[#4ade80] hover:bg-[#14532d]"
              onClick={() => { setFlow("idle"); setError(null); }}
            >
              Done
            </Button>
          </div>
          {error && <p className="mt-2 text-xs text-[#f87171]">{error}</p>}
        </CardContent>
      </Card>
    );
  }

  return null;
}

// ── Cluster backup card ───────────────────────────────────────────────────────

function ClusterBackupCard({ lastSnapshot }: { lastSnapshot: string | null }) {
  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div
            className="mt-0.5 rounded-md p-1.5 flex-shrink-0"
            style={{ background: lastSnapshot ? "#1a2e1a" : "#2d2a1a" }}
          >
            <Database
              className="h-4 w-4"
              style={{ color: lastSnapshot ? "#4ade80" : "#fbbf24" }}
              strokeWidth={1.75}
            />
          </div>
          <div className="flex-1">
            <p className="text-sm font-medium text-[#fafafa]">Cluster Backup (daily)</p>
            <p className="text-xs text-[#71717a] mt-0.5">
              {lastSnapshot
                ? `Last backup ${timeAgo(lastSnapshot)} — runs daily at 02:00 UTC`
                : "No backup yet — runs daily at 02:00 UTC"}
            </p>
          </div>
          {lastSnapshot && <ResultBadge result="Successful" />}
        </div>
      </CardContent>
    </Card>
  );
}

// ── PVC card ──────────────────────────────────────────────────────────────────

function PvcCard({ pvc }: { pvc: PvcStatus }) {
  const isFailed = pvc.result.toLowerCase() === "failed";
  const serviceName = serviceNameFromNamespace(pvc.namespace);
  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div
            className="mt-0.5 rounded-md p-1.5 flex-shrink-0"
            style={{ background: isFailed ? "#2d1a1a" : "#1a2e1a" }}
          >
            <Database
              className="h-4 w-4"
              style={{ color: isFailed ? "#f87171" : "#4ade80" }}
              strokeWidth={1.75}
            />
          </div>
          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2">
              <p className="text-sm font-medium text-[#fafafa]">{serviceName}</p>
              <ResultBadge result={pvc.result} />
            </div>
            {pvc.last_sync_time ? (
              <p className="text-xs text-[#71717a] mt-0.5">
                Last backup {timeAgo(pvc.last_sync_time)}
                {pvc.last_sync_duration && ` · ${pvc.last_sync_duration}`}
              </p>
            ) : (
              <p className="text-xs text-[#52525b] mt-0.5">No backup yet</p>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Enable card ───────────────────────────────────────────────────────────────

function EnableCard({ onEnable }: { onEnable: () => Promise<void> }) {
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
          <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0" style={{ background: "#2d2a1a" }}>
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
                disabled={busy}
                className="bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium text-sm h-8 px-3"
              >
                {busy ? (
                  <><RefreshCw className="h-3.5 w-3.5 mr-1.5 animate-spin" />Enabling…</>
                ) : (
                  "Enable Backups"
                )}
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

function DrBanner({
  lostCount,
  onStart,
}: {
  lostCount: number;
  onStart: () => Promise<void>;
}) {
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handle() {
    setStarting(true);
    setError(null);
    try { await onStart(); } catch (e) {
      setError(e instanceof Error ? e.message : "Failed");
    } finally { setStarting(false); }
  }

  return (
    <div className="rounded-lg border border-[#7f1d1d] bg-[#1c0a0a] p-4 space-y-3">
      <div className="flex items-start justify-between gap-4 flex-wrap">
        <div>
          <p className="text-sm font-semibold text-[#f87171] flex items-center gap-2">
            <AlertTriangle className="h-4 w-4" />
            Disk failure detected
          </p>
          <p className="text-xs text-[#71717a] mt-1">
            {lostCount} volume{lostCount !== 1 ? "s" : ""} lost. Restore all data from the last cloud backup.
          </p>
        </div>
        <Button
          onClick={handle}
          disabled={starting}
          className="flex-shrink-0 bg-[#dc2626] hover:bg-[#b91c1c] text-white border-0 text-sm h-9 px-4"
        >
          {starting ? (
            <><RefreshCw className="h-3.5 w-3.5 mr-1.5 animate-spin" />Starting…</>
          ) : (
            "Restore All from Cloud"
          )}
        </Button>
      </div>
      {error && <p className="text-xs text-[#f87171]">{error}</p>}
    </div>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

export function BackupsPage() {
  const [s3Status, setS3Status] = useState<{ provisioned: boolean } | null>(null);
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [drActive, setDrActive] = useState(false);

  const load = useCallback(async () => {
    const [s3Res, statusRes, drRes] = await Promise.all([
      fetch("/api/backups/s3").then(r => r.json()).catch(() => ({ provisioned: false })),
      fetch("/api/backups/status").then(r => r.json()).catch(() => null),
      fetch("/api/backups/dr/status").then(r => r.json()).catch(() => null),
    ]);
    setS3Status(s3Res as { provisioned: boolean });
    setBackupStatus(statusRes as BackupStatus | null);
    const dr = drRes as DrStatusResponse | null;
    setDrActive(!!dr && dr.total > 0 && !dr.all_complete);
    setLoading(false);
  }, []);

  useEffect(() => { void load(); }, [load]);

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok) throw new Error(await res.text() || `Server error ${res.status}`);
    await load();
  }

  async function handleDrStart() {
    const res = await fetch("/api/backups/dr/start", { method: "POST" });
    if (!res.ok) throw new Error(await res.text());
    const data = await res.json() as { started: string[] };
    if (data.started.length === 0) {
      throw new Error("No lost volumes found — cluster may already be healthy.");
    }
  }

  const lostCount = backupStatus?.pvcs.filter(
    p => p.pvc_phase === "Lost" || p.pvc_phase === "NotFound"
  ).length ?? 0;

  const currentNamespaces = new Set(backupStatus?.pvcs.map(p => p.namespace) ?? []);

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Backups</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Daily encrypted backups to Backblaze B2. Restore your data if disks fail or services break.
        </p>
      </div>

      {loading ? (
        <div className="space-y-3">
          <Card><CardContent className="pt-5 pb-5"><Shimmer className="h-14 w-full" /></CardContent></Card>
          <Card><CardContent className="pt-5 pb-5"><Shimmer className="h-14 w-full" /></CardContent></Card>
        </div>
      ) : !s3Status?.provisioned ? (
        <EnableCard onEnable={handleEnable} />
      ) : (
        <div className="space-y-3">
          {/* Auto-detected disk failure */}
          {lostCount > 0 && !drActive && (
            <DrBanner lostCount={lostCount} onStart={handleDrStart} />
          )}

          {/* User-initiated restore */}
          <RestorePanel currentNamespaces={currentNamespaces} />

          <div className="pt-2 pb-1">
            <p className="text-xs font-medium text-[#52525b] uppercase tracking-wider">Backup Status</p>
          </div>

          {/* Cluster backup */}
          <ClusterBackupCard lastSnapshot={backupStatus?.etcd_last_snapshot ?? null} />

          {/* Per-service PVC backups */}
          {backupStatus?.pvcs && backupStatus.pvcs.length > 0 ? (
            backupStatus.pvcs.map(pvc => (
              <PvcCard key={`${pvc.namespace}/${pvc.pvc}`} pvc={pvc} />
            ))
          ) : (
            <Card>
              <CardContent className="pt-5 pb-5">
                <p className="text-sm text-[#71717a]">
                  No service backups configured. Click "Enable Backups" above to set them up.
                </p>
              </CardContent>
            </Card>
          )}

          <div className="flex justify-end">
            <Button
              onClick={() => void load()}
              variant="outline"
              className="h-8 px-3 text-xs border-[#3f3f46] text-[#71717a] hover:text-[#fafafa]"
            >
              <RefreshCw className="h-3.5 w-3.5 mr-1.5" />
              Refresh
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
