import {
  ArrowRightLeft,
  OctagonAlert,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";
import { Card } from "@/components/ui/card";
import { Banner } from "@/components/ui/feedback";
import {
  AnimatedList,
  Collapse,
  RollingNumber,
  Swap,
} from "@/components/motion";
import { formatBytes } from "@/lib/format";
import {
  isVisible,
  jobOf,
  movementCopy,
  needsAttentionEverywhere,
  overallProgress,
  percent,
  timeLeft,
  useMovement,
} from "@/lib/movement";
import type { JobCopy, MovementTone } from "@/lib/movement";
import { cn } from "@/lib/utils";

const TONE: Record<
  MovementTone,
  { bar: string; icon: typeof ShieldCheck; iconClass: string; wrap: string }
> = {
  calm: {
    bar: "bg-primary",
    icon: ShieldCheck,
    iconClass: "text-success",
    wrap: "",
  },
  warning: {
    bar: "bg-warning",
    icon: ShieldAlert,
    iconClass: "text-warning",
    wrap: "border-warning/25",
  },
  error: {
    bar: "bg-danger",
    icon: OctagonAlert,
    iconClass: "text-danger",
    wrap: "border-danger/25",
  },
};

function JobRow({ copy, showLabel }: { copy: JobCopy; showLabel: boolean }) {
  const { job } = copy;
  return (
    <div className="mt-4">
      {showLabel && (
        <div className="flex items-center gap-2 text-sm text-fg">
          <span
            aria-hidden
            className={cn("h-2 w-2 shrink-0 rounded-full", TONE[copy.tone].bar)}
          />
          {copy.label}
        </div>
      )}
      <div className={cn("flex items-center gap-3", showLabel && "mt-2")}>
        <div className="h-2 flex-1 overflow-hidden rounded-full bg-surface-3">
          <div
            className={cn(
              "h-full rounded-full transition-[width,background-color] duration-700",
              TONE[copy.tone].bar,
            )}
            style={{ width: `${Math.max(percent(job.progress), 1.5)}%` }}
          />
        </div>
        <span className="shrink-0 text-sm tabular-nums text-fg-muted">
          {job.unit === "bytes" ? (
            <>
              <RollingNumber value={job.moved_bytes} format={formatBytes} /> of{" "}
              <RollingNumber value={job.to_move_bytes} format={formatBytes} />
            </>
          ) : (
            <>
              <RollingNumber value={Math.round(percent(job.progress))} />%
            </>
          )}
        </span>
      </div>
      {copy.note && <p className="mt-1.5 text-sm text-fg-muted">{copy.note}</p>}
    </div>
  );
}

export function DataMovementCard({ className }: { className?: string }) {
  const { data } = useMovement();
  const copy = data ? movementCopy(data) : null;
  const Icon = TONE[copy?.tone ?? "calm"].icon;

  return (
    <Collapse open={isVisible(data) && copy !== null} className={className}>
      {data && copy && (
        <Card
          className={cn(
            "p-6 transition-colors duration-300",
            TONE[copy.tone].wrap,
          )}
        >
          <div className="flex items-start gap-3">
            <ArrowRightLeft className="mt-0.5 h-5 w-5 shrink-0 text-fg-muted" />
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
                <p className="font-medium text-fg">
                  <Swap id={copy.headline}>{copy.headline}</Swap>
                </p>
                {copy.jobs.length > 0 && (
                  <span className="text-sm text-fg-muted">
                    <Swap id={timeLeft(data.eta_secs)}>
                      {timeLeft(data.eta_secs)}
                    </Swap>
                  </span>
                )}
              </div>
              <AnimatedList items={copy.jobs} keyOf={(j) => j.job.kind}>
                {(j) => <JobRow copy={j} showLabel={copy.jobs.length > 1} />}
              </AnimatedList>
              {copy.safety && (
                <p className="mt-4 flex items-start gap-2 text-sm text-fg-muted">
                  <Icon
                    className={cn(
                      "mt-0.5 h-4 w-4 shrink-0",
                      TONE[copy.tone].iconClass,
                    )}
                  />
                  <Swap id={copy.safety}>{copy.safety}</Swap>
                </p>
              )}
            </div>
          </div>
        </Card>
      )}
    </Collapse>
  );
}

export function DrainProgress() {
  const { data } = useMovement();
  const move = jobOf(data, "move");
  if (!move || move.progress === null) return null;
  return (
    <span className="text-fg-subtle">
      {" · "}
      <RollingNumber value={Math.round(percent(move.progress))} />% moved,{" "}
      <Swap id={timeLeft(move.eta_secs)}>{timeLeft(move.eta_secs)}</Swap>
    </span>
  );
}

export function MovementBanner({ className }: { className?: string }) {
  const { data } = useMovement();
  const copy = data ? movementCopy(data) : null;
  return (
    <Collapse
      open={needsAttentionEverywhere(data) && copy !== null}
      className={className}
    >
      {copy && (
        <Banner tone="error" title={copy.headline}>
          {copy.safety}
        </Banner>
      )}
    </Collapse>
  );
}

export function MovementSummary() {
  const { data } = useMovement();
  if (!isVisible(data)) return null;
  const copy = movementCopy(data);
  if (!copy) return null;
  const progress = overallProgress(data);
  if (copy.jobs.length === 0 || progress === null) return <>{copy.headline}</>;
  return (
    <>
      {copy.jobs.length > 1 ? "Reorganising files" : "Moving files"},{" "}
      <RollingNumber value={Math.round(percent(progress))} />%
    </>
  );
}
