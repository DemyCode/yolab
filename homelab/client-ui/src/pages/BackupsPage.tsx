import { useEffect, useRef, useState, useCallback } from "react";
import { Database, RefreshCw, CheckCircle, AlertCircle, Clock, AlertTriangle } from "lucide-react";
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

type DrPhase = "none" | "detected" | "restoring" | "complete" | "applying" | "done";

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

type RestoreStep = "idle" | "running" | "ready" | "applying" | "done";

function PvcCard({ pvc }: { pvc: PvcStatus }) {
  const [step, setStep] = useState<RestoreStep>("idle");
  const [error, setError] = useState<string | null>(null);
  const pollRef = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (pollRef.current !== null) clearInterval(pollRef.current);
    };
  }, []);

  async function handleRestore() {
    if (
      !confirm(
        `Restore "${pvc.pvc}" from the last cloud backup?\n\n` +
        `The app will be stopped and its current data replaced with the backed-up version.`
      )
    ) return;

    setStep("running");
    setError(null);

    try {
      const res = await fetch(`/api/backups/restore/${pvc.namespace}/${pvc.pvc}/emergency`, {
        method: "POST",
      });
      if (!res.ok) throw new Error(await res.text());

      pollRef.current = window.setInterval(async () => {
        try {
          const s = await fetch(
            `/api/backups/restore/${pvc.namespace}/${pvc.pvc}/emergency/status`
          ).then((r) => r.json()) as { found: boolean; result?: string };
          if (s.result?.toLowerCase() === "successful") {
            clearInterval(pollRef.current!);
            pollRef.current = null;
            setStep("ready");
          } else if (s.result?.toLowerCase() === "failed") {
            clearInterval(pollRef.current!);
            pollRef.current = null;
            setStep("idle");
            setError("Restore failed — check VolSync logs.");
          }
        } catch {
          // network blip — keep polling
        }
      }, 5000);
    } catch (e) {
      setStep("idle");
      setError(e instanceof Error ? e.message : "Failed to start restore");
    }
  }

  async function handleApply() {
    setStep("applying");
    try {
      const res = await fetch(
        `/api/backups/restore/${pvc.namespace}/${pvc.pvc}/emergency/apply`,
        { method: "POST" }
      );
      if (!res.ok) throw new Error(await res.text());
      setStep("done");
    } catch (e) {
      setStep("ready");
      setError(e instanceof Error ? e.message : "Apply failed");
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
              <div className="flex items-center gap-2 flex-shrink-0">
                <ResultBadge result={pvc.result} />
                <Button
                  onClick={handleRestore}
                  disabled={!pvc.last_sync_time || step !== "idle"}
                  variant="outline"
                  className="h-7 px-2.5 text-xs border-[#3f3f46] text-[#a1a1aa] hover:text-[#fafafa] hover:border-[#6b7280]"
                >
                  Restore
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

            {step === "running" && (
              <div className="mt-3 flex items-center gap-2 text-xs text-[#fbbf24]">
                <RefreshCw className="h-3 w-3 animate-spin" />
                Pulling data from cloud backup… this may take 10–30 min.
              </div>
            )}
            {step === "ready" && (
              <div className="mt-3 flex items-center justify-between gap-3">
                <p className="text-xs text-[#4ade80]">
                  Data ready. Click Apply to restart the app on the restored data.
                </p>
                <Button
                  onClick={handleApply}
                  className="h-7 px-3 text-xs bg-[#15803d] hover:bg-[#16a34a] text-white border-0 flex-shrink-0"
                >
                  Apply
                </Button>
              </div>
            )}
            {step === "applying" && (
              <div className="mt-3 flex items-center gap-2 text-xs text-[#a78bfa]">
                <RefreshCw className="h-3 w-3 animate-spin" />
                Starting app on restored data…
              </div>
            )}
            {step === "done" && (
              <div className="mt-3 flex items-center gap-2 text-xs text-[#4ade80]">
                <CheckCircle className="h-3 w-3" />
                App restarted on restored data.
              </div>
            )}
            {error && (
              <p className="mt-2 text-xs text-[#f87171]">{error}</p>
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

// ── Disaster-recovery banner ──────────────────────────────────────────────────

function DisasterRecoveryBanner({
  phase,
  restores,
  done,
  total,
  failed,
  lostCount,
  onStart,
  onApply,
  error,
}: {
  phase: DrPhase;
  restores: DrRestoreItem[];
  done: number;
  total: number;
  failed: number;
  lostCount: number;
  onStart: () => Promise<void>;
  onApply: () => Promise<void>;
  error: string | null;
}) {
  const [starting, setStarting] = useState(false);
  const [applying, setApplying] = useState(false);

  if (phase === "none") return null;

  const isSuccess = phase === "done" || phase === "complete";

  async function handleStart() {
    setStarting(true);
    try { await onStart(); } finally { setStarting(false); }
  }
  async function handleApply() {
    setApplying(true);
    try { await onApply(); } finally { setApplying(false); }
  }

  return (
    <div
      className="rounded-lg border p-4 space-y-3"
      style={{
        borderColor: isSuccess ? "#14532d" : "#7f1d1d",
        background: isSuccess ? "#0f1f0f" : "#1c0a0a",
      }}
    >
      {phase === "detected" && (
        <div className="flex items-start justify-between gap-4 flex-wrap">
          <div>
            <p className="text-sm font-semibold text-[#f87171] flex items-center gap-2">
              <AlertTriangle className="h-4 w-4" />
              Disaster Recovery Mode
            </p>
            <p className="text-xs text-[#71717a] mt-1">
              {lostCount} PVC{lostCount !== 1 ? "s" : ""} lost — your apps are down.
              Restore all data from the last cloud backup.
            </p>
          </div>
          <Button
            onClick={handleStart}
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
      )}

      {phase === "restoring" && (
        <>
          <p className="text-sm font-semibold text-[#fbbf24] flex items-center gap-2">
            <RefreshCw className="h-4 w-4 animate-spin" />
            Restoring data… {done}/{total} complete
          </p>
          <div className="space-y-1.5">
            {restores.map((r) => (
              <div key={`${r.namespace}/${r.pvc}`} className="flex items-center gap-2 text-xs">
                {r.result.toLowerCase() === "successful" ? (
                  <CheckCircle className="h-3.5 w-3.5 text-[#4ade80] flex-shrink-0" />
                ) : r.result.toLowerCase() === "failed" ? (
                  <AlertCircle className="h-3.5 w-3.5 text-[#f87171] flex-shrink-0" />
                ) : (
                  <RefreshCw className="h-3.5 w-3.5 text-[#fbbf24] animate-spin flex-shrink-0" />
                )}
                <span className="text-[#a1a1aa] font-mono">{r.pvc}</span>
                <span className="text-[#52525b]">{r.namespace}</span>
                <span
                  style={{
                    color:
                      r.result.toLowerCase() === "successful"
                        ? "#4ade80"
                        : r.result.toLowerCase() === "failed"
                        ? "#f87171"
                        : "#fbbf24",
                  }}
                >
                  {r.result}
                </span>
              </div>
            ))}
          </div>
          {failed > 0 && (
            <p className="text-xs text-[#f87171]">
              {failed} restore{failed !== 1 ? "s" : ""} failed — check VolSync logs.
            </p>
          )}
        </>
      )}

      {phase === "complete" && (
        <div className="flex items-start justify-between gap-4 flex-wrap">
          <div>
            <p className="text-sm font-semibold text-[#4ade80] flex items-center gap-2">
              <CheckCircle className="h-4 w-4" />
              All data restored
            </p>
            <p className="text-xs text-[#71717a] mt-1">
              Click Apply to start all services on the restored data.
            </p>
          </div>
          <Button
            onClick={handleApply}
            disabled={applying}
            className="flex-shrink-0 bg-[#15803d] hover:bg-[#16a34a] text-white border-0 text-sm h-9 px-4"
          >
            {applying ? (
              <><RefreshCw className="h-3.5 w-3.5 mr-1.5 animate-spin" />Applying…</>
            ) : (
              "Apply — Start All Services"
            )}
          </Button>
        </div>
      )}

      {phase === "applying" && (
        <p className="text-sm text-[#a78bfa] flex items-center gap-2">
          <RefreshCw className="h-4 w-4 animate-spin" />
          Starting all services on restored data…
        </p>
      )}

      {phase === "done" && (
        <p className="text-sm font-semibold text-[#4ade80] flex items-center gap-2">
          <CheckCircle className="h-4 w-4" />
          All services restored successfully.
        </p>
      )}

      {error && <p className="text-xs text-[#f87171]">{error}</p>}
    </div>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

export function BackupsPage() {
  const [s3Status, setS3Status] = useState<S3Status | null>(null);
  const [backupStatus, setBackupStatus] = useState<BackupStatus | null>(null);
  const [loading, setLoading] = useState(true);

  // DR state
  const [drPhase, setDrPhase] = useState<DrPhase>("none");
  const [drRestores, setDrRestores] = useState<DrRestoreItem[]>([]);
  const [drDone, setDrDone] = useState(0);
  const [drTotal, setDrTotal] = useState(0);
  const [drFailed, setDrFailed] = useState(0);
  const [drError, setDrError] = useState<string | null>(null);
  const drPollRef = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (drPollRef.current !== null) clearInterval(drPollRef.current);
    };
  }, []);

  const startDrPolling = useCallback(() => {
    if (drPollRef.current !== null) return;
    drPollRef.current = window.setInterval(async () => {
      try {
        const s = await fetch("/api/backups/dr/status").then((r) => r.json()) as DrStatusResponse;
        setDrRestores(s.restores);
        setDrDone(s.done);
        setDrTotal(s.total);
        setDrFailed(s.failed);
        if (s.all_complete) {
          clearInterval(drPollRef.current!);
          drPollRef.current = null;
          setDrPhase("complete");
        }
      } catch {
        // network blip — keep polling
      }
    }, 5000);
  }, []);

  const load = useCallback(async () => {
    const [s3Res, statusRes, drStatusRes] = await Promise.all([
      fetch("/api/backups/s3").then((r) => r.json()).catch(() => ({ provisioned: false })),
      fetch("/api/backups/status").then((r) => r.json()).catch(() => null),
      fetch("/api/backups/dr/status").then((r) => r.json()).catch(() => null),
    ]);
    setS3Status(s3Res as S3Status);
    setBackupStatus(statusRes as BackupStatus | null);
    setLoading(false);

    // Detect DR mode: in-progress restores take precedence over Lost PVCs.
    const drStatus = drStatusRes as DrStatusResponse | null;
    if (drStatus && drStatus.total > 0) {
      setDrRestores(drStatus.restores);
      setDrDone(drStatus.done);
      setDrTotal(drStatus.total);
      setDrFailed(drStatus.failed);
      setDrPhase((prev) => {
        if (prev === "applying" || prev === "done") return prev;
        return drStatus.all_complete ? "complete" : "restoring";
      });
      if (!drStatus.all_complete) startDrPolling();
    } else if ((statusRes as BackupStatus)?.dr_mode) {
      setDrPhase((prev) => (prev === "none" ? "detected" : prev));
    }
  }, [startDrPolling]);

  useEffect(() => { void load(); }, [load]);

  async function handleEnable() {
    const res = await fetch("/api/backups/s3/enable", { method: "POST" });
    if (!res.ok) {
      const text = await res.text();
      throw new Error(text || `Server error ${res.status}`);
    }
    await load();
  }

  async function handleDrStart() {
    setDrError(null);
    const res = await fetch("/api/backups/dr/start", { method: "POST" });
    if (!res.ok) throw new Error(await res.text());
    const data = await res.json() as { started: string[]; skipped: string[] };
    if (data.started.length === 0) {
      throw new Error("No Lost PVCs found to restore — cluster may already be healthy.");
    }
    setDrPhase("restoring");
    startDrPolling();
  }

  async function handleDrApply() {
    setDrError(null);
    setDrPhase("applying");
    try {
      const res = await fetch("/api/backups/dr/apply", { method: "POST" });
      if (!res.ok) throw new Error(await res.text());
      const data = await res.json() as { applied: string[]; errors: string[] };
      if (data.errors?.length > 0) {
        setDrError(`Some errors: ${data.errors.join(", ")}`);
      }
      setDrPhase("done");
    } catch (e) {
      setDrPhase("complete");
      setDrError(e instanceof Error ? e.message : "Apply failed");
    }
  }

  const lostCount = backupStatus?.pvcs.filter(
    (p) => p.pvc_phase === "Lost" || p.pvc_phase === "NotFound"
  ).length ?? 0;

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
          {drPhase !== "none" && (
            <DisasterRecoveryBanner
              phase={drPhase}
              restores={drRestores}
              done={drDone}
              total={drTotal}
              failed={drFailed}
              lostCount={lostCount}
              onStart={handleDrStart}
              onApply={handleDrApply}
              error={drError}
            />
          )}

          {(drPhase === "none" || drPhase === "done") && (
            <>
              <EtcdCard lastSnapshot={backupStatus?.etcd_last_snapshot ?? null} />

              {backupStatus?.pvcs && backupStatus.pvcs.length > 0 ? (
                backupStatus.pvcs.map((pvc) => (
                  <PvcCard
                    key={`${pvc.namespace}/${pvc.pvc}`}
                    pvc={pvc}
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
            </>
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
