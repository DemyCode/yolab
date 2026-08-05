import { useEffect, useRef, type ReactNode } from "react";
import { X } from "lucide-react";
import { cn } from "@/lib/utils";
import { Button } from "./button";

/**
 * One component, two shapes: a sheet that rises from the bottom on a phone and
 * a centred panel on a desktop. Same content, same code — this is most of what
 * "derive a phone app from the same source" costs in practice.
 */
export function Sheet({
  open,
  onClose,
  title,
  subtitle,
  children,
  footer,
  wide,
}: {
  open: boolean;
  onClose: () => void;
  title?: ReactNode;
  subtitle?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
}) {
  const panelRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    // Stop the page behind from scrolling under the sheet.
    const previous = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.removeEventListener("keydown", onKey);
      document.body.style.overflow = previous;
    };
  }, [open, onClose]);

  useEffect(() => {
    if (open) panelRef.current?.focus();
  }, [open]);

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center sm:items-center"
      role="dialog"
      aria-modal="true"
    >
      <div
        className="absolute inset-0 bg-black/40 animate-fade-in"
        onClick={onClose}
        aria-hidden
      />
      <div
        ref={panelRef}
        tabIndex={-1}
        className={cn(
          "relative flex max-h-[92vh] w-full flex-col bg-surface shadow-[var(--shadow-pop)] animate-sheet-in focus:outline-none",
          "rounded-t-3xl sm:rounded-card",
          wide ? "sm:max-w-2xl" : "sm:max-w-md",
          "sm:mx-4",
        )}
      >
        {/* Grab handle: purely a phone affordance, hidden on desktop. */}
        <div className="mx-auto mt-3 h-1 w-10 shrink-0 rounded-full bg-border-strong sm:hidden" />

        {(title || subtitle) && (
          <div className="flex items-start gap-3 px-5 pb-3 pt-4">
            <div className="min-w-0 flex-1">
              {title && (
                <h2 className="text-lg font-semibold leading-tight text-fg">
                  {title}
                </h2>
              )}
              {subtitle && (
                <p className="mt-0.5 text-sm text-fg-muted">{subtitle}</p>
              )}
            </div>
            <button
              onClick={onClose}
              className="-mr-1 shrink-0 rounded-lg p-2 text-fg-muted hover:bg-surface-2 hover:text-fg"
              aria-label="Close"
            >
              <X className="h-5 w-5" />
            </button>
          </div>
        )}

        <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-5">
          {children}
        </div>

        {footer && (
          <div className="shrink-0 border-t border-border p-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
            {footer}
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * Confirmation for things that cannot be undone.
 *
 * Takes the consequence as its body text rather than a generic "Are you sure?".
 * "Are you sure?" is answered yes by everyone; "This deletes the 4,200 photos
 * in Immich" is not.
 */
export function ConfirmDialog({
  open,
  onClose,
  onConfirm,
  title,
  body,
  confirmLabel = "Continue",
  destructive,
  busy,
}: {
  open: boolean;
  onClose: () => void;
  onConfirm: () => void;
  title: string;
  body: ReactNode;
  confirmLabel?: string;
  destructive?: boolean;
  busy?: boolean;
}) {
  return (
    <Sheet open={open} onClose={onClose} title={title}>
      <div className="text-sm text-fg-muted">{body}</div>
      <div className="mt-6 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
        <Button variant="secondary" onClick={onClose} disabled={busy}>
          Cancel
        </Button>
        <Button
          variant={destructive ? "danger" : "primary"}
          onClick={onConfirm}
          loading={busy}
        >
          {confirmLabel}
        </Button>
      </div>
    </Sheet>
  );
}
