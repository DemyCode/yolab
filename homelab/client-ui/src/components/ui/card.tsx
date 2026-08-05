import { cn } from "@/lib/utils";
import type { HTMLAttributes } from "react";

export function Card({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "rounded-card border border-border bg-surface shadow-[var(--shadow-card)]",
        className,
      )}
      {...props}
    />
  );
}

export function CardHeader({
  className,
  ...props
}: HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cn("flex flex-col gap-1 p-5 pb-0", className)} {...props} />
  );
}

export function CardTitle({
  className,
  ...props
}: HTMLAttributes<HTMLHeadingElement>) {
  return (
    <h3
      className={cn("text-base font-semibold text-fg leading-tight", className)}
      {...props}
    />
  );
}

export function CardDescription({
  className,
  ...props
}: HTMLAttributes<HTMLParagraphElement>) {
  return <p className={cn("text-sm text-fg-muted", className)} {...props} />;
}

export function CardContent({
  className,
  ...props
}: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("p-5", className)} {...props} />;
}

/**
 * A labelled row inside a settings card — label left, value right, optional
 * chevron. Most of Settings is made of these, which is the point: the
 * operator pages become a list of plain sentences rather than a dashboard.
 */
export function Row({
  label,
  value,
  hint,
  action,
  className,
}: {
  label: string;
  value?: React.ReactNode;
  hint?: string;
  action?: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex items-center gap-4 px-5 py-4 border-b border-border last:border-0",
        className,
      )}
    >
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium text-fg">{label}</div>
        {hint && <div className="mt-0.5 text-sm text-fg-muted">{hint}</div>}
      </div>
      {value !== undefined && (
        <div className="shrink-0 text-sm text-fg-muted tabular-nums">
          {value}
        </div>
      )}
      {action}
    </div>
  );
}
