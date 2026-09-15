import { useEffect, useState, useCallback } from "react";
import {
  Database,
  RefreshCw,
  CheckCircle,
  AlertTriangle,
  RotateCcw,
  KeyRound,
  Copy,
} from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";

// ── Types ─────────────────────────────────────────────────────────────────────

type BackupSetState = "running" | "restorable" | "crashed";

interface BackupSet {
  id: string;
  triggered_by: string;
  started_at: string;
  finished_at?: string | null;
  snapshot_id?: string | null;
  error?: string | null;
  state: BackupSetState;
  services?: { instance_name: string; pvc_count: number }[];
}

interface OperationState {
  backing_up: boolean;
  restoring: boolean;
  backup_run: BackupSet | null;
  restore_run: unknown;
  last_backup: BackupSet | null;
  last_ok_age_hours: number | null;
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

function serviceName(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function BackupSetCard({ set: backupSet }: { set: BackupSet }) {
  const isRunning = backupSet.state === "running";
  const isRestorable = backupSet.state === "restorable";
  const services = backupSet.services ?? [];

  return (
    <Card
      className={
        isRunning ? "border-primary/30 bg-primary-soft/20" : "border-border"
      }
    >
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
              {formatDate(backupSet.started_at)}
            </span>
            <span className="ml-2 text-xs text-fg-subtle">
              {timeAgo(backupSet.started_at)}
            </span>
            <span
              className={`ml-2 text-xs ${isRunning ? "text-primary" : isRestorable ? "text-success" : "text-warning"}`}
            >
              {setStateLabel(backupSet.state)}
            </span>
            {backupSet.triggered_by === "schedule" && (
              <span className="ml-2 text-xs text-fg-muted">· automatic</span>
            )}
          </div>
        </div>

        {isRunning && (
          <p className="mt-3 text-xs text-fg-muted">
            Your files stay available the whole time. A large folder can take a
            while the first time it is copied — nothing is wrong, and it will
            not be cut short for taking long.
          </p>
        )}

        {!isRunning && backupSet.error && (
          <p className="mt-2 text-xs text-danger">{backupSet.error}</p>
        )}

        {isRestorable && services.length > 0 && (
          <div className="mt-3 flex flex-wrap gap-1.5">
            {services.map((s) => (
              <span
                key={s.instance_name}
                className="inline-flex items-center gap-1.5 rounded border border-border bg-surface-2 px-2 py-1 text-xs text-fg-muted"
              >
                {serviceName(s.instance_name)}
                {s.pvc_count > 0 && (
                  <span className="text-fg-subtle">
                    · {s.pvc_count} volume{s.pvc_count === 1 ? "" : "s"}
                  </span>
                )}
              </span>
            ))}
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
      /* clipboard unavailable */
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
      /* network blip */
    }
  }

  const load = useCallback(async () => {
    const [s3Res, runsRes] = await Promise.all([
      fetch("/api/backups/s3")
        .then((r) => r.json())
        .catch(() => ({ provisioned: false })),
      fetch("/api/backups/runs")
        .then((r) => r.json())
        .catch(() => []),
    ]);
    setS3Status(s3Res as { provisioned: boolean });
    if (Array.isArray(runsRes)) {
      setSets(runsRes as BackupSet[]);
    }
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The list is polled separately from the one-shot `load`, so a backup that is
  // running appears as a row the moment it starts and flips to Restorable (with
  // its services) the moment it finishes — without re-fetching the s3 status.
  const loadRuns = useCallback(async () => {
    try {
      const runsRes = await fetch("/api/backups/runs").then((r) => r.json());
      if (Array.isArray(runsRes)) setSets(runsRes as BackupSet[]);
    } catch {
      /* network blip */
    }
  }, []);

  const pollOpState = useCallback(async () => {
    try {
      const s = (await fetch("/api/backups/state").then((r) =>
        r.json(),
      )) as OperationState;
      setOpState(s);
    } catch {
      /* network blip */
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    const id = window.setInterval(() => {
      if (cancelled) return;
      void pollOpState();
      void loadRuns();
    }, 5000);
    void pollOpState();
    void loadRuns();
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [pollOpState, loadRuns]);

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
            Restore individual apps from their own pages.
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

      {loading ? (
        <div className="space-y-3">
          <Card>
            <CardContent className="pt-5 pb-5">
              <Shimmer className="h-14 w-full" />
            </CardContent>
          </Card>
        </div>
      ) : !s3Status?.provisioned ? (
        <EnableCard onEnable={handleEnable} disabled={false} />
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
            <BackupSetCard key={s.id} set={s} />
          ))}
        </div>
      )}
    </div>
  );
}
