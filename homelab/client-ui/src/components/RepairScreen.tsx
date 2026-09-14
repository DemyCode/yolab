import { useState } from "react";
import { CheckCircle, Circle, RefreshCw, Wrench, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  RECOVERY_STEP_LABELS,
  dismissRecovery,
  finishedRecently,
  recoveryDismissed,
  stepState,
  useRecoveryStatus,
  type RecoveryStatus,
} from "@/lib/recovery";

function Bar({ percent, className }: { percent: number; className?: string }) {
  return (
    <div
      className={cn(
        "h-2 w-full overflow-hidden rounded-full bg-surface-3",
        className,
      )}
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent}
    >
      <div
        className="h-full rounded-full bg-primary transition-[width] duration-700"
        style={{ width: `${Math.max(0, Math.min(100, percent))}%` }}
      />
    </div>
  );
}

type Recovery = NonNullable<RecoveryStatus["recovery"]>;

function Steps({ r }: { r: Recovery }) {
  return (
    <ol className="space-y-3">
      {r.steps.map((step) => {
        const state = stepState(r.steps, r.step, step, r.running);
        const p = state === "current" ? r.step_progress : null;
        return (
          <li key={step} className="flex gap-3">
            {state === "done" ? (
              <CheckCircle className="mt-0.5 h-5 w-5 shrink-0 text-success" />
            ) : state === "current" ? (
              <RefreshCw className="mt-0.5 h-5 w-5 shrink-0 animate-spin text-primary" />
            ) : (
              <Circle className="mt-0.5 h-5 w-5 shrink-0 text-fg-subtle" />
            )}
            <div className="min-w-0 flex-1">
              <p
                className={cn(
                  "text-base",
                  state === "current" && "font-medium text-fg",
                  state === "done" && "text-fg-muted",
                  state === "pending" && "text-fg-subtle",
                )}
              >
                {RECOVERY_STEP_LABELS[step]}
              </p>
              {p && p.total > 0 && (
                <div className="mt-2 space-y-1">
                  <Bar percent={(100 * p.done) / p.total} />
                  <p className="text-sm text-fg-muted">
                    {p.detail ?? `${p.done} of ${p.total}`}
                    <span className="text-fg-subtle">
                      {" "}
                      · {Math.round((100 * p.done) / p.total)}%
                    </span>
                  </p>
                </div>
              )}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

function Apps({ r }: { r: Recovery }) {
  if (r.apps.length === 0 && !r.not_restored?.length) return null;
  return (
    <div className="space-y-2 border-t border-border pt-5">
      <p className="text-sm font-medium text-fg">Your apps</p>
      <ul className="space-y-1.5 text-sm">
        {r.apps.map((a) => (
          <li key={a.namespace} className="flex items-start gap-2">
            {a.outcome?.result === "restored" ? (
              <CheckCircle className="mt-0.5 h-4 w-4 shrink-0 text-success" />
            ) : a.outcome?.result === "failed" ? (
              <XCircle className="mt-0.5 h-4 w-4 shrink-0 text-danger" />
            ) : (
              <Circle className="mt-0.5 h-4 w-4 shrink-0 text-fg-subtle" />
            )}
            <span>
              <span className="text-fg">{a.instance_name}</span>
              {a.outcome?.result === "restored" && (
                <span className="text-fg-muted"> — back from backup</span>
              )}
              {a.outcome?.result === "failed" && (
                <span className="text-danger"> — {a.outcome.error}</span>
              )}
            </span>
          </li>
        ))}
      </ul>
      {r.not_restored && r.not_restored.length > 0 && (
        <p className="text-sm text-fg-muted">
          Not in the backup, so not coming back:{" "}
          <span className="text-fg">{r.not_restored.join(", ")}</span>
        </p>
      )}
    </div>
  );
}

/** The whole interface while storage is being repaired, and once, right after. */
export function RepairScreen({
  recovery: r,
  onDone,
}: {
  recovery: Recovery;
  onDone: () => void;
}) {
  const failed = r.apps.filter((a) => a.outcome?.result === "failed").length;
  return (
    <div className="min-h-dvh bg-bg px-4 py-10 sm:py-16">
      <div className="mx-auto max-w-2xl space-y-8">
        <header className="space-y-3 text-center">
          <div className="mx-auto flex h-14 w-14 items-center justify-center rounded-full bg-primary-soft">
            {r.running ? (
              <Wrench className="h-7 w-7 text-primary" />
            ) : (
              <CheckCircle className="h-7 w-7 text-success" />
            )}
          </div>
          <h1 className="font-display text-3xl text-fg sm:text-4xl">
            {r.running ? "Repairing your home server" : "Repair finished"}
          </h1>
          <p className="text-fg-muted">
            {r.running
              ? "Storage is being rebuilt and your apps reinstalled from your latest backup. This page updates by itself — you can close it and come back."
              : failed > 0
                ? `Everything that could come back is back. ${failed} app${failed === 1 ? "" : "s"} could not be restored — you can try again from ${failed === 1 ? "its" : "their"} page.`
                : "Your storage is healthy again and your apps are back from your latest backup."}
          </p>
        </header>

        <div className="space-y-2">
          <div className="flex items-baseline justify-between">
            <span className="text-sm text-fg-muted">
              {r.running ? RECOVERY_STEP_LABELS[r.step] : "Done"}
            </span>
            <span className="font-display text-4xl tabular-nums text-fg">
              {r.percent}%
            </span>
          </div>
          <Bar percent={r.percent} className="h-3" />
        </div>

        <div className="space-y-6 rounded-card border border-border bg-surface p-6">
          <Steps r={r} />
          <Apps r={r} />
        </div>

        {!r.running && (
          <div className="flex justify-center">
            <Button onClick={onDone}>Continue to my apps</Button>
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * Shows the repair screen in place of everything else while a recovery runs, and
 * its summary once afterwards. A status that cannot be read never blocks the app.
 */
export function RepairGate({ children }: { children: React.ReactNode }) {
  const status = useRecoveryStatus();
  const [dismissedNow, setDismissedNow] = useState<number | null>(null);
  const [openedAt] = useState(() => Date.now() / 1000);
  const r = status.data?.recovery ?? null;

  const show =
    r !== null &&
    (r.running ||
      (finishedRecently(r, openedAt) &&
        dismissedNow !== r.started_at &&
        !recoveryDismissed(r.started_at)));

  if (!show || !r) return <>{children}</>;
  return (
    <RepairScreen
      recovery={r}
      onDone={() => {
        dismissRecovery(r.started_at);
        setDismissedNow(r.started_at);
      }}
    />
  );
}
