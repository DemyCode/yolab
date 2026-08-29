import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";
import type { HTMLAttributes } from "react";

const badgeVariants = cva(
  "inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-medium",
  {
    variants: {
      variant: {
        neutral: "bg-surface-2 text-fg-muted border border-border",
        primary: "bg-primary-soft text-primary border border-primary/20",
        success: "bg-success-soft text-success border border-success/20",
        warning: "bg-warning-soft text-warning border border-warning/20",
        danger: "bg-danger-soft text-danger border border-danger/20",
        /** Border only — for labels that should recede, like a version number. */
        outline: "border border-border text-fg-muted",
        /** Filled, no border — for counts and inert metadata. */
        muted: "bg-surface-2 text-fg-muted",
      },
    },
    defaultVariants: { variant: "neutral" },
  },
);

export interface BadgeProps
  extends HTMLAttributes<HTMLSpanElement>, VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return (
    <span className={cn(badgeVariants({ variant, className }))} {...props} />
  );
}

/**
 * A coloured dot.
 *
 * Deliberately has no "everything is fine" state that renders visibly — the
 * caller passes `tone="ok"` and gets nothing. Permanent green ticks train
 * people to scan for status they cannot interpret; silence is the success
 * case, and a dot appearing means something actually wants them.
 */
export function StatusDot({
  tone,
  pulse,
  className,
}: {
  tone: "ok" | "busy" | "warn" | "error";
  pulse?: boolean;
  className?: string;
}) {
  if (tone === "ok") return null;
  const color =
    tone === "error"
      ? "bg-danger"
      : tone === "warn"
        ? "bg-warning"
        : "bg-primary";
  return (
    <span
      className={cn(
        "inline-block h-2.5 w-2.5 rounded-full ring-2 ring-surface",
        color,
        pulse && "animate-pulse",
        className,
      )}
    />
  );
}
