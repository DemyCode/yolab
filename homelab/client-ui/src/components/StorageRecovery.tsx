import { useState } from "react";
import { Link } from "react-router-dom";
import { AlertTriangle, CheckCircle, RefreshCw, XCircle } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Banner } from "@/components/ui/feedback";
import { ConfirmDialog } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import {
  RECOVERY_STEPS,
  finishedRecently,
  recoveryNeedsAttention,
  useRecoveryStatus,
  type RecoveryPreview,
  type RecoveryStatus,
} from "@/lib/recovery";

function formatDate(iso: string): string {
  return new Date(iso).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function AppList({ names }: { names: string[] }) {
  if (names.length === 0) return <span className="text-fg-subtle">none</span>;
  return <span className="font-medium text-fg">{names.join(", ")}</span>;
}

function RecoverDialog({
  open,
  onClose,
  onStarted,
}: {
  open: boolean;
  onClose: () => void;
  onStarted: () => void;
}) {
  const preview = useApi<RecoveryPreview>(
    open ? "storage-recovery-preview" : null,
    "/api/storage/recovery/preview",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function start() {
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/storage/recovery");
      onStarted();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not start the recovery");
    } finally {
      setBusy(false);
    }
  }

  const p = preview.data;
  return (
    <ConfirmDialog
      open={open}
      onClose={onClose}
      onConfirm={() => void start()}
      title="Recover from backup?"
      confirmLabel="Reset storage and recover"
      destructive
      busy={busy || preview.loading}
      body={
        <div className="space-y-3">
          <p>
            Storage is reset and every app is reinstalled from your latest
            backup. This cannot be undone: reconnecting the lost disk afterwards
            brings nothing back.
          </p>
          {preview.loading && <p>Reading your backup…</p>}
          {preview.error && !p && (
            <p className="text-danger">
              Your backup could not be read right now.
            </p>
          )}
          {p && (
            <>
              <p>
                {p.backup_taken_at
                  ? `Latest backup: ${formatDate(p.backup_taken_at)}. Every app goes back to that moment.`
                  : "There is no backup to recover from."}
              </p>
              <p>
                Come back: <AppList names={p.restored} />
              </p>
              <p>
                Do not come back (not in the backup):{" "}
                <AppList names={p.not_restored} />
              </p>
            </>
          )}
          {error && <p className="text-danger">{error}</p>}
        </div>
      }
    />
  );
}

/** The Backups page section: what was lost, the button, and the recovery's progress. */
export function StorageRecoveryCard() {
  const status = useRecoveryStatus();
  const [confirming, setConfirming] = useState(false);
  // Read once when the page opens: "finished this week" does not need to tick.
  const [openedAt] = useState(() => Date.now() / 1000);
  const s = status.data;
  if (!s) return null;

  const r = s.recovery;
  const recentlyFinished = finishedRecently(r, openedAt);

  if (r?.running) {
    const current = RECOVERY_STEPS.findIndex((x) => x.step === r.step);
    return (
      <Card className="border-primary/30">
        <CardContent className="space-y-3 pt-5 pb-5">
          <p className="flex items-center gap-2 text-sm font-medium text-fg">
            <RefreshCw className="h-4 w-4 animate-spin text-primary" />
            Recovering from backup
          </p>
          <ol className="space-y-1 text-sm">
            {RECOVERY_STEPS.map((x, i) => (
              <li
                key={x.step}
                className={
                  i < current
                    ? "text-fg-subtle line-through"
                    : i === current
                      ? "font-medium text-fg"
                      : "text-fg-muted"
                }
              >
                {x.label}
              </li>
            ))}
          </ol>
          <RecoveryApps recovery={r} />
        </CardContent>
      </Card>
    );
  }

  if (s.loss?.needs_recovery) {
    return (
      <>
        <Card className="border-danger/30 bg-danger-soft">
          <CardContent className="space-y-3 pt-5 pb-5">
            <p className="flex items-center gap-2 text-sm font-medium text-danger">
              <AlertTriangle className="h-4 w-4" />A disk holding app data is
              gone
            </p>
            <p className="text-sm text-fg-muted">
              If you can, reconnect it: everything comes back on its own. If it
              is not coming back, recover from your latest backup. Backups are
              paused until then, so nothing damaged replaces a good backup.
            </p>
            <Button variant="danger" onClick={() => setConfirming(true)}>
              Recover health from backup
            </Button>
          </CardContent>
        </Card>
        <RecoverDialog
          open={confirming}
          onClose={() => setConfirming(false)}
          onStarted={() => void status.refresh()}
        />
      </>
    );
  }

  if (recentlyFinished && r) {
    return (
      <Card>
        <CardContent className="space-y-3 pt-5 pb-5">
          <p className="flex items-center gap-2 text-sm font-medium text-fg">
            <CheckCircle className="h-4 w-4 text-success" />
            Recovered from backup
          </p>
          <RecoveryApps recovery={r} />
        </CardContent>
      </Card>
    );
  }
  return null;
}

function RecoveryApps({
  recovery,
}: {
  recovery: NonNullable<RecoveryStatus["recovery"]>;
}) {
  return (
    <div className="space-y-2 text-sm">
      {recovery.apps.length > 0 && (
        <ul className="space-y-1">
          {recovery.apps.map((a) => (
            <li key={a.namespace} className="flex items-start gap-2">
              {a.outcome?.result === "restored" ? (
                <CheckCircle className="mt-0.5 h-4 w-4 shrink-0 text-success" />
              ) : a.outcome?.result === "failed" ? (
                <XCircle className="mt-0.5 h-4 w-4 shrink-0 text-danger" />
              ) : (
                <RefreshCw className="mt-0.5 h-4 w-4 shrink-0 text-fg-subtle" />
              )}
              <span>
                <span className="text-fg">{a.instance_name}</span>
                {a.outcome?.result === "failed" && (
                  <span className="text-danger"> — {a.outcome.error}</span>
                )}
              </span>
            </li>
          ))}
        </ul>
      )}
      {recovery.not_restored && recovery.not_restored.length > 0 && (
        <p className="text-fg-muted">
          Not in the backup, so not reinstalled:{" "}
          <AppList names={recovery.not_restored} />
        </p>
      )}
    </div>
  );
}

/** One line on the home page pointing at the Backups page while it matters. */
export function StorageRecoveryBanner({ className }: { className?: string }) {
  const status = useRecoveryStatus();
  if (!recoveryNeedsAttention(status.data)) return null;
  const running = status.data?.recovery?.running;
  return (
    <Banner
      tone={running ? "info" : "error"}
      title={
        running ? "Recovering from backup" : "A disk holding app data is gone"
      }
      className={className}
      action={
        <Link
          to="/box/backups"
          className={buttonClass({ size: "sm", variant: "secondary" })}
        >
          {running ? "See progress" : "Recover"}
        </Link>
      }
    >
      {running
        ? "Your apps are being reinstalled from your latest backup."
        : "Reconnect it to get everything back, or recover your apps from backup."}
    </Banner>
  );
}
