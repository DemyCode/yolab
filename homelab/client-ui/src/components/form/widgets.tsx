import { useState } from "react";
import type { WidgetProps } from "@rjsf/utils";
import { Eye, EyeOff, RefreshCw } from "lucide-react";
import { Input, Toggle } from "@/components/ui/input";
import { generateSecret } from "@/lib/format";
import { cn } from "@/lib/utils";

/**
 * The widgets the catalog already asks for.
 *
 * Every chart ships an RJSF uiSchema in its `yolab.io/uischema` annotation —
 * 55 `TunnelWidget`, 20 `PasswordWidget`, 5 `ui:autofocus` across the catalog.
 * Those names only mean something to RJSF, and the previous hand-rolled
 * renderer ignored all of them, re-deriving the same intent by matching field
 * names against /pass|secret|key|token/. A chart author writing
 * `ui:widget: PasswordWidget` was being quietly overruled by a regex.
 *
 * So these are not decoration: they are the reason the catalog's uiSchema
 * exists at all.
 */

/**
 * The app's web address.
 *
 * More than a text box: the subdomain becomes a real DNS name, so it shows the
 * full URL as you type. Seeing `https://photos.<your-domain>` is the difference
 * between "subdomain" meaning something and not.
 */
export function TunnelWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, autofocus, id, options } = props;
  const domain = (options?.domain as string) ?? "";
  const v = typeof value === "string" ? value : "";

  return (
    <div className="space-y-1.5">
      <Input
        id={id}
        value={v}
        autoFocus={autofocus}
        disabled={disabled || readonly}
        onChange={(e) =>
          // A subdomain is a DNS label: lowercase, alphanumeric and hyphens.
          // Correcting as they type beats rejecting on submit.
          onChange(
            e.target.value
              .toLowerCase()
              .replace(/[^a-z0-9-]/g, "-")
              .replace(/^-+/, ""),
          )
        }
      />
      {v && domain && (
        <p className="truncate text-sm text-fg-muted">
          https://{v}.{domain}
        </p>
      )}
    </div>
  );
}

/**
 * A generated credential.
 *
 * Pre-filled rather than blank: nobody wants to invent a password for a
 * database they will never type it into, and a blank field invites `admin`.
 * Hidden by default, with reveal and regenerate, because it is worth copying
 * before install and worthless afterwards.
 */
export function PasswordWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, id, schema } = props;
  const [shown, setShown] = useState(false);
  const v = typeof value === "string" ? value : "";

  const regenerate = () =>
    onChange(generateSecret(Math.max(24, (schema.minLength as number) ?? 0)));

  return (
    <div className="flex items-center gap-2">
      <Input
        id={id}
        type={shown ? "text" : "password"}
        value={v}
        disabled={disabled || readonly}
        onChange={(e) => onChange(e.target.value)}
        className="flex-1 font-mono"
      />
      <button
        type="button"
        aria-label={shown ? "Hide" : "Show"}
        onClick={() => setShown((s) => !s)}
        className="rounded-md p-2 text-fg-muted hover:bg-surface-2"
      >
        {shown ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
      </button>
      <button
        type="button"
        aria-label="Generate a new one"
        onClick={regenerate}
        className="rounded-md p-2 text-fg-muted hover:bg-surface-2"
      >
        <RefreshCw className="h-4 w-4" />
      </button>
    </div>
  );
}

/** RJSF's checkbox, rendered as the switch used everywhere else in the UI. */
export function CheckboxWidget(props: WidgetProps) {
  const { value, onChange, label, schema, disabled, readonly } = props;
  return (
    <Toggle
      label={label || (schema.title as string) || ""}
      help={schema.description as string | undefined}
      checked={Boolean(value)}
      onChange={(v) => !(disabled || readonly) && onChange(v)}
    />
  );
}

/** Long-form text — used for things like a list of logins, one per line. */
export function TextareaWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, id, placeholder } = props;
  return (
    <textarea
      id={id}
      value={typeof value === "string" ? value : ""}
      placeholder={placeholder}
      disabled={disabled || readonly}
      rows={4}
      onChange={(e) => onChange(e.target.value)}
      className={cn(
        "w-full rounded-lg border border-border bg-surface px-3 py-2 font-mono text-sm",
        "text-fg placeholder:text-fg-subtle focus:border-primary focus:outline-none",
      )}
    />
  );
}
