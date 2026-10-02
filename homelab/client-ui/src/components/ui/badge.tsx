import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";
import type { HTMLAttributes } from "react";

const badgeVariants = cva(
  "inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-medium transition-colors duration-300",
  {
    variants: {
      variant: {
        neutral: "bg-surface-2 text-fg-muted border border-border",
        primary: "bg-primary-soft text-primary border border-primary/20",
        success: "bg-success-soft text-success border border-success/20",
        warning: "bg-warning-soft text-warning border border-warning/20",
        danger: "bg-danger-soft text-danger border border-danger/20",
        outline: "border border-border text-fg-muted",
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

export function StatusDot({
  tone,
  pulse,
  className,
}: {
  tone: "ok" | "busy" | "warn" | "error";
  pulse?: boolean;
  className?: string;
}) {
  const color =
    tone === "error"
      ? "bg-danger"
      : tone === "warn"
        ? "bg-warning"
        : tone === "busy"
          ? "bg-primary"
          : "bg-success";
  return (
    <span
      aria-hidden={tone === "ok" || undefined}
      className={cn(
        "inline-block h-2.5 w-2.5 rounded-full ring-2 ring-surface transition-[background-color,opacity,transform] duration-300",
        color,
        tone === "ok" && "scale-50 opacity-0",
        pulse && tone !== "ok" && "animate-pulse",
        className,
      )}
    />
  );
}
