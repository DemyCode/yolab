import { useState, type ReactNode } from "react";
import { Check, ChevronDown, ChevronRight, Copy } from "lucide-react";
import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils";

const insetRing =
  "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-primary";

const focusRing =
  "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary";

export function Section({
  title,
  action,
  children,
}: {
  title: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="mt-8">
      <div className="mb-2 flex min-h-8 items-center justify-between gap-3 px-1">
        <h2 className="text-sm font-semibold text-fg-muted">{title}</h2>
        {action}
      </div>
      <Card className="divide-y divide-border overflow-hidden p-0">
        {children}
      </Card>
    </section>
  );
}

export function Row({
  label,
  detail,
  trailing,
  onClick,
  danger,
  disabled,
  children,
}: {
  label: ReactNode;
  detail?: ReactNode;
  trailing?: ReactNode;
  onClick?: () => void;
  danger?: boolean;
  disabled?: boolean;
  children?: ReactNode;
}) {
  const body = (
    <>
      <div className="min-w-0 flex-1">
        <div
          className={cn(
            "text-sm font-medium",
            danger ? "text-danger" : "text-fg",
          )}
        >
          {label}
        </div>
        {detail && (
          <div className="mt-0.5 break-words text-sm text-fg-muted">
            {detail}
          </div>
        )}
      </div>
      {trailing}
      {onClick && trailing === undefined && (
        <ChevronRight className="h-5 w-5 shrink-0 text-fg-subtle" />
      )}
    </>
  );

  if (onClick) {
    return (
      <button
        type="button"
        onClick={onClick}
        disabled={disabled}
        className={cn(
          "flex w-full items-center gap-3 px-5 py-4 text-left transition-colors hover:bg-surface-2",
          insetRing,
          "disabled:pointer-events-none disabled:opacity-50",
        )}
      >
        {body}
      </button>
    );
  }
  return (
    <div className="px-5 py-4">
      <div className="flex items-center gap-3">{body}</div>
      {children}
    </div>
  );
}

export function DisclosureRow({
  label,
  detail,
  children,
}: {
  label: ReactNode;
  detail?: ReactNode;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        className={cn(
          "flex w-full items-center gap-3 px-5 py-4 text-left transition-colors hover:bg-surface-2",
          insetRing,
        )}
      >
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium text-fg">{label}</div>
          {detail && (
            <div className="mt-0.5 text-sm text-fg-muted">{detail}</div>
          )}
        </div>
        <ChevronDown
          className={cn(
            "h-5 w-5 shrink-0 text-fg-subtle transition-transform",
            open && "rotate-180",
          )}
        />
      </button>
      {open && <div className="space-y-3 px-5 pb-5">{children}</div>}
    </div>
  );
}

export function RowAction({
  children,
  onClick,
  disabled,
}: {
  children: ReactNode;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={cn(
        "inline-flex shrink-0 items-center gap-1.5 rounded-control px-2.5 py-1.5 text-sm font-medium text-primary transition-colors hover:bg-primary-soft",
        focusRing,
        "disabled:pointer-events-none disabled:opacity-60",
      )}
    >
      {children}
    </button>
  );
}

export function IconButton({
  label,
  onClick,
  href,
  children,
}: {
  label: string;
  onClick?: () => void;
  href?: string;
  children: ReactNode;
}) {
  const className = cn(
    "inline-flex shrink-0 items-center justify-center rounded-control p-2 text-fg-muted transition-colors hover:bg-surface-2 hover:text-fg",
    focusRing,
  );
  if (href) {
    return (
      <a
        href={href}
        target="_blank"
        rel="noopener noreferrer"
        className={className}
        aria-label={label}
      >
        {children}
      </a>
    );
  }
  return (
    <button
      type="button"
      onClick={onClick}
      className={className}
      aria-label={label}
    >
      {children}
    </button>
  );
}

export function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <IconButton
      label={copied ? `${label} copied` : `Copy ${label}`}
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(value);
          setCopied(true);
          setTimeout(() => setCopied(false), 1600);
          // eslint-disable-next-line no-empty
        } catch {}
      }}
    >
      {copied ? (
        <Check className="h-4 w-4 text-success" />
      ) : (
        <Copy className="h-4 w-4" />
      )}
    </IconButton>
  );
}

export function ValueRow({
  label,
  value,
  copy,
  trailing,
}: {
  label: ReactNode;
  value: string;
  copy?: boolean;
  trailing?: ReactNode;
}) {
  return (
    <div className="px-5 py-4">
      <div className="text-sm font-medium text-fg">{label}</div>
      <div className="mt-1 flex items-center gap-1">
        <code className="min-w-0 flex-1 truncate font-mono text-sm text-fg-muted" title={value}>
          {value}
        </code>
        {trailing}
        {copy && <CopyButton value={value} label={String(label)} />}
      </div>
    </div>
  );
}
