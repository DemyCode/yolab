import { useEffect, useState, useCallback } from "react";
import { Database, RefreshCw, RotateCcw, CheckCircle, AlertCircle, Clock } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";

// ── Types ─────────────────────────────────────────────────────────────────────

interface PvcStatus {
  namespace: string;
  pvc: string;
  last_sync_time: string | null;
  last_sync_duration: string | null;
  result: string;
}

interface BackupStatus {
  pvcs: PvcStatus[];
  etcd_last_snapshot: string | null;
}

interface S3Status {
  provisioned: boolean;
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

// ── Etcd card ─────────────────────────────────────────────────────────────────

function EtcdCard({ lastSnapshot }: { lastSnapshot: string | null }) {
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
            <p className="text-sm font-medium text-[#fafafa]">Cluster State (etcd)</p>
            <p className="text-xs text-[#71717a] mt-0.5">
              {lastSnapshot
                ? `Last snapshot ${timeAgo(lastSnapshot)} — daily at 02:00 UTC`
                : "No snapshot yet — runs daily at 02:00 UTC"}
            </p>
          </div>
          {lastSnapshot && (
            <ResultBadge result="Successful" />
          )}
        </div>
      </CardContent>
    </Card>
  );
}

// ── PVC card ──────────────────────────────────────────────────────────────────

function PvcCard({
  pvc,
  onRestore,
}: {
  pvc: PvcStatus;
  onRestore: (namespace: string, pvc: string) => Promise<void>;
}) {
  const [restoring, setRestoring] = useState(false);
  const [restoreError, setRestoreError] = useState<string | null>(null);
  const [restoreResult, setRestoreResult] = useState<string | null>(null);

  async function handleRestore() {
    if (!confirm(`Restore ${pvc.pvc} from last backup?\n\nThis will create a new PVC with the restored data.`)) return;
    setRestoring(true);
    setRestoreError(null);
    try {
      await onRestore(pvc.namespace, pvc.pvc);
      setRestoreResult("Restore started — check status in a few minutes.");
    } catch (e) {
      setRestoreError(e instanceof Error ? e.message : "Failed");
    } finally {
      setRestoring(false);
    }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="flex items-start gap-3">
          <div
            className="mt-0.5 rounded-md p-1.5 flex-shrink-0"
            style={{ background: pvc.result.toLowerCase() === "failed" ? "#2d1a1a" : "#1a2e1a" }}
          >
            <Database
              className="h-4 w-4"
              style={{ color: pvc.result.toLowerCase() === "failed" ? "#f87171" : "#4ade80" }}
              strokeWidth={1.75}
            />
          </div>
          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-4 flex-wrap">
              <div className="min-w-0">
                <p className="text-sm font-medium text-[#fafafa] truncate">{pvc.pvc}</p>
                <p className="text-xs text-[#71717a] mt-0.5">{pvc.namespace}</p>
              </div>
              <div className="flex items-center gap-3 flex-shrink-0">
                <ResultBadge result={pvc.result} />
                <Button
                  onClick={handleRestore}
                  disabled={restoring || !pvc.last_sync_time}
                  variant="outline"
                  className="h-7 px-2.5 text-xs border-[#3f3f46] text-[#a1a1aa] hover:text-[#fafafa] hover:border-[#6b7280]"
                >
                  {restoring ? (
                    <RefreshCw className="h-3 w-3 animate-spin" />
                  ) : (
                    <><RotateCcw className="h-3 w-3 mr-1" />Restore</>
                  )}
                </Button>
              </div>
            </div>

            {pvc.last_sync_time && (
              <div className="mt-2 grid grid-cols-2 gap-x-4 text-xs font-mono">
                <div className="flex gap-2">
                  <span className="text-[#52525b]">Last sync</span>
                  <span className="text-[#a1a1aa]">{timeAgo(pvc.last_sync_time)}</span>
                </div>
                {pvc.last_sync_duration && (
                  <div className="flex gap-2">
                    <span className="text-[#52525b]">Duration</span>
                    <span className="text-[#a1a1aa]">{pvc.last_sync_duration}</span>
                  </div>
                )}
              </div>
            )}

            {restoreError && (
              <p className="mt-2 text-xs text-[#f87171]">{restoreError}</p>
            )}
            {restoreResult && (
              <p className="mt-2 text-xs text-[#4ade80]">{restoreResult}</p>
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

// ── Page ──────────────────────────────────────────────────────────────────────

export function BackupsPage() {
  const [s3Status, setS3Status] = useState<S3Status | null>(null);
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    const [s3Res, statusRes] = await Promise.all([
      fetch("/api/backups/s3").then((r) => r.json()).catch(() => ({ provisioned: false })),
      fetch("/api/backups/status").then((r) => r.json()).catch(() => null),
    ]);
    setS3Status(s3Res as S3Status);
    setBackupStatus(statusRes as BackupStatus | null);
    setLoading(false);
  }, []);

  useEffect(() => { void load(); }, [load]);

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok) {
      const text = await res.text();
      throw new Error(text || `Server error ${res.status}`);
    }
    await load();
  }

  async function handleRestore(namespace: string, pvc: string) {
    const res = await fetch(`/api/backups/restore/${namespace}/${pvc}`, { method: "POST" });
    if (!res.ok) {
      const text = await res.text();
      throw new Error(text || `Server error ${res.status}`);
    }
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Backups</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Daily encrypted backups to Backblaze B2 via VolSync. Restore your data if disks or nodes fail.
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
          <EtcdCard lastSnapshot={backupStatus?.etcd_last_snapshot ?? null} />

          {backupStatus?.pvcs && backupStatus.pvcs.length > 0 ? (
            backupStatus.pvcs.map((pvc) => (
              <PvcCard
                key={`${pvc.namespace}/${pvc.pvc}`}
                pvc={pvc}
                onRestore={handleRestore}
              />
            ))
          ) : (
            <Card>
              <CardContent className="pt-5 pb-5">
                <p className="text-sm text-[#71717a]">
                  No PVC backup sources found. Click Enable Backups to configure them.
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
