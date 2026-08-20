import { useId, useState } from "react";
import { Check, Copy, Eye, EyeOff, RefreshCw } from "lucide-react";
import { cn } from "@/lib/utils";
import { generateSecret } from "@/lib/format";
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

/**
 * A password the person never had to invent.
 *
 * Every required-without-a-default field in the whole catalog is a secret
 * (`admin_password`, `app_secret`, `app_key`, …). Asking someone to make one up
 * is both the biggest single obstacle in the install flow and worse security
 * than generating it — so the value arrives already filled, and the only jobs
 * left are "copy it" and, if they insist, "use my own".
 */
export function GeneratedSecret({
  value,
  onChange,
  label,
  help,
  minLength,
}: {
  value: string;
  onChange: (v: string) => void;
  label: string;
  help?: string;
  minLength?: number;
}) {
  const id = useId();
  const [revealed, setRevealed] = useState(false);
  const [copied, setCopied] = useState(false);
  const [custom, setCustom] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      // Clipboard is blocked outside a secure context; revealing the value is
      // the useful fallback since they can still select it by hand.
      setRevealed(true);
    }
  }

  if (custom) {
    return (
      <Field
        label={label}
        htmlFor={id}
        help={minLength ? `At least ${minLength} characters.` : help}
        error={
          minLength && value.length > 0 && value.length < minLength
            ? `Needs at least ${minLength} characters.`
            : null
        }
      >
        <Input
          id={id}
          type="text"
          value={value}
          autoComplete="new-password"
          onChange={(e) => onChange(e.target.value)}
        />
      </Field>
    );
  }

  return (
    <div className="space-y-1.5">
      <span className="block text-sm font-medium text-fg">{label}</span>
      <div className="flex items-center gap-2 rounded-xl border border-border bg-surface-2 px-3.5 py-2.5">
        <code className="flex-1 truncate font-mono text-sm text-fg">
          {revealed ? value : "•".repeat(Math.min(value.length, 24))}
        </code>
        <button
          type="button"
          onClick={() => setRevealed((r) => !r)}
          className="rounded-lg p-2 text-fg-muted hover:bg-surface-3 hover:text-fg"
          aria-label={revealed ? "Hide password" : "Show password"}
        >
          {revealed ? (
            <EyeOff className="h-4 w-4" />
          ) : (
            <Eye className="h-4 w-4" />
          )}
        </button>
        <button
          type="button"
          onClick={() => void copy()}
          className="rounded-lg p-2 text-fg-muted hover:bg-surface-3 hover:text-fg"
          aria-label="Copy password"
        >
          {copied ? (
            <Check className="h-4 w-4 text-success" />
          ) : (
            <Copy className="h-4 w-4" />
          )}
        </button>
        <button
          type="button"
          onClick={() => onChange(generateSecret(Math.max(24, minLength ?? 0)))}
          className="rounded-lg p-2 text-fg-muted hover:bg-surface-3 hover:text-fg"
          aria-label="Generate a different password"
        >
          <RefreshCw className="h-4 w-4" />
        </button>
      </div>
      <p className="text-sm text-fg-muted">
        We made this one for you — copy it somewhere safe.{" "}
        <button
          type="button"
          className="text-primary underline underline-offset-2"
          onClick={() => {
            setCustom(true);
            onChange("");
          }}
        >
          Use my own instead
        </button>
      </p>
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
      className="flex cursor-pointer items-center gap-4"
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
          "relative h-7 w-12 shrink-0 rounded-full transition-colors",
          checked ? "bg-primary" : "bg-surface-3",
        )}
      >
        <span
          className={cn(
            "absolute top-0.5 h-6 w-6 rounded-full bg-white shadow transition-transform",
            checked ? "translate-x-[1.375rem]" : "translate-x-0.5",
          )}
        />
      </button>
    </div>
  );
}
