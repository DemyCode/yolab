import { cn } from "@/lib/utils";
import type {
  InputHTMLAttributes,
  ReactNode,
  SelectHTMLAttributes,
} from "react";

const control =
  "w-full h-11 rounded-xl border border-border bg-surface px-3.5 text-sm text-fg " +
  "placeholder:text-fg-subtle transition-colors " +
  "focus:outline-none focus:border-primary focus:ring-2 focus:ring-primary/20 " +
  "disabled:opacity-60";

export function Input({
  className,
  ...props
}: InputHTMLAttributes<HTMLInputElement>) {
  return <input className={cn(control, className)} {...props} />;
}

export function Select({
  className,
  children,
  ...props
}: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select className={cn(control, "pr-9", className)} {...props}>
      {children}
    </select>
  );
}

/** Label + help text + error, wrapped around any control. */
export function Field({
  label,
  help,
  error,
  children,
  htmlFor,
}: {
  label: string;
  help?: string;
  error?: string | null;
  children: ReactNode;
  htmlFor?: string;
}) {
  return (
    <div className="space-y-1.5">
      <label htmlFor={htmlFor} className="block text-sm font-medium text-fg">
        {label}
      </label>
      {children}
      {error ? (
        <p className="text-sm text-danger">{error}</p>
      ) : help ? (
        <p className="text-sm text-fg-muted">{help}</p>
      ) : null}
    </div>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
  help,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  help?: string;
}) {
  return (
    // A <div>, not a <label>. A <label> forwards its own click to the labelable
    // control inside it — and <button> is labelable — so clicking the switch
    // fired onChange twice: once from the button, once re-dispatched by the
    // label. The value flipped and immediately flipped back, which reads as a
    // toggle that does nothing. Clicking the text still toggles, via the
    // wrapper's own handler.
    <div
      className="flex w-full cursor-pointer items-center gap-4"
      onClick={() => onChange(!checked)}
    >
      <span className="min-w-0 flex-1">
        <span className="block text-sm font-medium text-fg">{label}</span>
        {help && <span className="block text-sm text-fg-muted">{help}</span>}
      </span>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        aria-label={label}
        // Stop the wrapper's handler from also firing — otherwise the button
        // and the div both toggle and cancel each other out.
        onClick={(e) => {
          e.stopPropagation();
          onChange(!checked);
        }}
        className={cn(
          // overflow-hidden so the knob is physically incapable of escaping the
          // track, whatever the transform resolves to.
          "relative h-7 w-12 shrink-0 overflow-hidden rounded-full transition-colors",
          checked ? "bg-primary" : "bg-surface-3",
        )}
      >
        <span
          className={cn(
            // `left-0.5` matters: an absolutely-positioned box with no `left`
            // falls back to its STATIC position, and the translate then stacks
            // on top of wherever that lands — which is how the knob ended up
            // outside its track. Anchored explicitly, the geometry is just
            // 2px + 20px = 22px, leaving the same 2px margin on the right that
            // it has on the left.
            "absolute left-0.5 top-0.5 h-6 w-6 rounded-full bg-white shadow transition-transform",
            checked ? "translate-x-5" : "translate-x-0",
          )}
        />
      </button>
    </div>
  );
}
