import { ArrowRightLeft, OctagonAlert, ShieldAlert, ShieldCheck } from "lucide-react";
import { Card } from "@/components/ui/card";
import { Banner } from "@/components/ui/feedback";
import { Collapse, RollingNumber, Swap } from "@/components/motion";
import { formatBytes } from "@/lib/format";
import {
  isVisible,
  movementCopy,
  needsAttentionEverywhere,
  percent,
  timeLeft,
  useMovement,
} from "@/lib/movement";
import type { MovementTone } from "@/lib/movement";
import { cn } from "@/lib/utils";
import type { Movement } from "@/types/storage";

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

function Progress({ m, tone }: { m: Movement; tone: MovementTone }) {
  return (
    <>
      <div className="mt-4 h-2 overflow-hidden rounded-full bg-surface-3">
        <div
          className={cn(
            "h-full rounded-full transition-[width,background-color] duration-700",
            TONE[tone].bar,
          )}
          style={{ width: `${Math.max(percent(m.progress), 1.5)}%` }}
        />
      </div>
      <p className="mt-2 flex flex-wrap items-center gap-x-2 text-sm text-fg-muted tabular-nums">
        {m.to_move_bytes > 0 && (
          <>
            <span>
              <RollingNumber value={m.moved_bytes} format={formatBytes} /> of{" "}
              <RollingNumber value={m.to_move_bytes} format={formatBytes} />{" "}
              moved
            </span>
            <span aria-hidden className="text-fg-subtle">
              ·
            </span>
          </>
        )}
        <Swap id={timeLeft(m.eta_secs)}>{timeLeft(m.eta_secs)}</Swap>
      </p>
    </>
  );
}

export function DataMovementCard({ className }: { className?: string }) {
  const { data } = useMovement();
  const visible = isVisible(data);
  const copy = data ? movementCopy(data) : null;
  const Icon = TONE[copy?.tone ?? "calm"].icon;

  return (
    <Collapse open={visible && copy !== null} className={className}>
      {data && copy && (
        <Card className={cn("p-6 transition-colors duration-300", TONE[copy.tone].wrap)}>
          <div className="flex items-start gap-3">
            <ArrowRightLeft className="mt-0.5 h-5 w-5 shrink-0 text-fg-muted" />
            <div className="min-w-0 flex-1">
              <p className="font-medium text-fg">
                <Swap id={copy.headline}>{copy.headline}</Swap>
              </p>
              {copy.showsProgress && <Progress m={data} tone={copy.tone} />}
              <p className="mt-3 flex items-start gap-2 text-sm text-fg-muted">
                <Icon
                  className={cn("mt-0.5 h-4 w-4 shrink-0", TONE[copy.tone].iconClass)}
                />
                <Swap id={copy.safety}>{copy.safety}</Swap>
              </p>
            </div>
          </div>
        </Card>
      )}
    </Collapse>
  );
}

export function DrainProgress() {
  const { data } = useMovement();
  if (!data || data.state === "settled" || data.progress === null) return null;
  return (
    <span className="text-fg-subtle">
      {" · "}
      <RollingNumber value={Math.round(percent(data.progress))} />% moved,{" "}
      <Swap id={timeLeft(data.eta_secs)}>{timeLeft(data.eta_secs)}</Swap>
    </span>
  );
}

export function MovementBanner({ className }: { className?: string }) {
  const { data } = useMovement();
  const show = needsAttentionEverywhere(data);
  const copy = data ? movementCopy(data) : null;
  return (
    <Collapse open={show && copy !== null} className={className}>
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
  if (!copy.showsProgress || data.progress === null) {
    return <>{copy.headline}</>;
  }
  return (
    <>
      Moving files, <RollingNumber value={Math.round(percent(data.progress))} />%
    </>
  );
}
