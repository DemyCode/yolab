import { useState } from "react";
import type { WidgetProps } from "@rjsf/utils";
import { Eye, EyeOff, RefreshCw } from "lucide-react";
import { Input, Toggle } from "@/components/ui/input";
import { generateSecret } from "@/lib/format";
import { cn } from "@/lib/utils";

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

export function YolabTokenWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, id } = props;
  const v = typeof value === "string" ? value : "";
  const [own, setOwn] = useState(v !== "");

  if (!own) {
    return (
      <div className="flex items-center justify-between gap-3">
        <p id={id} className="text-sm text-fg-muted">
          <span className="mr-2 font-mono tracking-widest text-fg">
            ••••••••
          </span>
          Uses this box&apos;s YoLab account
        </p>
        <button
          type="button"
          disabled={disabled || readonly}
          onClick={() => setOwn(true)}
          className="rounded-control px-2 py-1 text-sm text-primary hover:bg-surface-2"
        >
          Use another token
        </button>
      </div>
    );
  }
  return (
    <div className="flex items-center gap-2">
      <Input
        id={id}
        type="password"
        value={v}
        autoComplete="off"
        disabled={disabled || readonly}
        onChange={(e) => onChange(e.target.value)}
        className="flex-1 font-mono"
      />
      <button
        type="button"
        disabled={disabled || readonly}
        onClick={() => {
          onChange(undefined);
          setOwn(false);
        }}
        className="rounded-control px-2 py-1 text-sm text-primary hover:bg-surface-2"
      >
        Use this box&apos;s
      </button>
    </div>
  );
}

export function PasswordWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, id, schema, options } = props;
  const [shown, setShown] = useState(false);
  const generates = options?.generate === true;
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
        className="rounded-control p-2 text-fg-muted hover:bg-surface-2"
      >
        {shown ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
      </button>
      {generates && (
        <button
          type="button"
          aria-label="Generate a new one"
          onClick={regenerate}
          className="rounded-control p-2 text-fg-muted hover:bg-surface-2"
        >
          <RefreshCw className="h-4 w-4" />
        </button>
      )}
    </div>
  );
}

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
        "w-full rounded-control border border-border bg-surface px-3 py-2 font-mono text-sm",
        "text-fg placeholder:text-fg-subtle focus:border-primary focus:outline-none",
      )}
    />
  );
}
