import {
  AlertTriangle,
  Info,
  Loader2,
  OctagonAlert,
  WifiOff,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { buttonClass } from "@/components/ui/button-variants";
import type { ReactNode } from "react";

export function Skeleton({ className }: { className?: string }) {
  return <div className={cn("shimmer rounded-xl bg-surface-2", className)} />;
}

export function Spinner({ className }: { className?: string }) {
  return (
    <Loader2
      className={cn("h-5 w-5 animate-spin text-fg-subtle", className)}
      aria-label="Loading"
    />
  );
}

export type Tone = "info" | "warning" | "error";

export function Banner({
  tone,
  title,
  children,
  action,
  className,
}: {
  tone: Tone;
  title: string;
  children?: ReactNode;
  action?: ReactNode;
  className?: string;
}) {
  const styles = {
    info: {
      wrap: "bg-primary-soft border-primary/20",
      icon: "text-primary",
      Icon: Info,
    },
    warning: {
      wrap: "bg-warning-soft border-warning/25",
      icon: "text-warning",
      Icon: AlertTriangle,
    },
    error: {
      wrap: "bg-danger-soft border-danger/25",
      icon: "text-danger",
      Icon: OctagonAlert,
    },
  }[tone];
  const { Icon } = styles;

  return (
    <div
      className={cn(
        "flex items-start gap-3 rounded-card border p-4",
        styles.wrap,
        className,
      )}
      role={tone === "error" ? "alert" : undefined}
    >
      <Icon className={cn("mt-0.5 h-5 w-5 shrink-0", styles.icon)} />
      <div className="min-w-0 flex-1">
        <p className="text-sm font-semibold text-fg">{title}</p>
        {children && (
          <div className="mt-1 text-sm text-fg-muted">{children}</div>
        )}
        {action && <div className="mt-3">{action}</div>}
      </div>
    </div>
  );
}

export function EmptyState({
  icon,
  title,
  body,
  action,
}: {
  icon?: ReactNode;
  title: string;
  body?: string;
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center justify-center px-6 py-16 text-center">
      {icon && (
        <div className="mb-4 flex h-14 w-14 items-center justify-center rounded-2xl bg-surface-2 text-fg-subtle">
          {icon}
        </div>
      )}
      <h3 className="font-display text-xl text-fg">{title}</h3>
      {body && <p className="mt-1.5 max-w-sm text-sm text-fg-muted">{body}</p>}
      {action && <div className="mt-6">{action}</div>}
    </div>
  );
}

export function ServiceTrouble({ onRetry }: { onRetry: () => void }) {
  return (
    <EmptyState
      icon={<WifiOff className="h-6 w-6" />}
      title="Having trouble reaching your services"
      body="This is usually brief — the box may just be busy for a moment. Try again in a bit."
      action={
        <button
          type="button"
          onClick={onRetry}
          className={buttonClass({ variant: "secondary" })}
        >
          Try again
        </button>
      }
    />
  );
}
