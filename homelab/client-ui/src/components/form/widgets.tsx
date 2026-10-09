import { useState } from "react";
import type { WidgetProps } from "@rjsf/utils";
import { Eye, EyeOff, RefreshCw } from "lucide-react";
import { Input, Select, Toggle } from "@/components/ui/input";
import { generateSecret } from "@/lib/format";
import { DEFAULT_SIZE_GIB, folderChoice, type Folder } from "@/lib/folders";
import { api } from "@/lib/api";
import {
  serviceChoice,
  serviceLabel,
  type ServiceInstance,
} from "@/lib/services";
import { useApi } from "@/lib/useResource";
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

const OWN = "own";
const BY_HAND = "url";

export function ServiceUrlWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, id, options, required } = props;
  const service = String(options?.service ?? "");
  const installed =
    useApi<ServiceInstance[]>(
      service ? `services:${service}` : null,
      `/api/services/${encodeURIComponent(service)}`,
    ).data ?? [];
  const v = typeof value === "string" ? value : "";
  const [typing, setTyping] = useState(false);
  const choice = typing
    ? ({ kind: "url", url: v } as const)
    : serviceChoice(v, installed, !required);
  const selected =
    choice.kind === "own"
      ? OWN
      : choice.kind === "installed"
        ? choice.url
        : BY_HAND;

  return (
    <div className="space-y-2">
      <Select
        id={id}
        value={selected}
        disabled={disabled || readonly}
        onChange={(e) => {
          const next = e.target.value;
          setTyping(next === BY_HAND);
          if (next === OWN) onChange(undefined);
          else if (next === BY_HAND) onChange(choice.kind === "url" ? v : "");
          else onChange(next);
        }}
      >
        {!required && <option value={OWN}>Run its own</option>}
        {installed.map((s) => (
          <option key={s.url} value={s.url}>
            Use {serviceLabel(s)}
          </option>
        ))}
        <option value={BY_HAND}>Another address…</option>
      </Select>
      {selected === BY_HAND && (
        <Input
          value={v}
          placeholder="http://192.168.1.20:8080"
          disabled={disabled || readonly}
          onChange={(e) => onChange(e.target.value || undefined)}
          className="font-mono"
        />
      )}
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

const INSIDE = "";
const NEW_FOLDER = "__new__";

export function FolderWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, id } = props;
  const folders = useApi<Folder[]>("folders", "/api/folders");
  const list = folders.data ?? [];
  const v = typeof value === "string" ? value : "";
  const choice = folderChoice(v, list);
  const [creating, setCreating] = useState(false);
  const [title, setTitle] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const off = disabled || readonly;

  async function create() {
    setBusy(true);
    setError(null);
    try {
      const made = await api.post<{ name: string }>("/api/folders", {
        title,
        size_gib: DEFAULT_SIZE_GIB,
      });
      await folders.refresh();
      onChange(made.name);
      setCreating(false);
      setTitle("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-2">
      <Select
        id={id}
        value={creating ? NEW_FOLDER : v}
        disabled={off}
        onChange={(e) => {
          const next = e.target.value;
          setCreating(next === NEW_FOLDER);
          if (next !== NEW_FOLDER) onChange(next === INSIDE ? undefined : next);
        }}
      >
        <option value={INSIDE}>Keep inside this app</option>
        {list.map((f) => (
          <option key={f.name} value={f.name}>
            {f.ready ? f.title : `${f.title} (being created)`}
          </option>
        ))}
        {choice.kind === "missing" && (
          <option value={choice.name}>{choice.name} (no longer exists)</option>
        )}
        <option value={NEW_FOLDER}>Create a new folder…</option>
      </Select>
      {creating && (
        <div className="flex items-center gap-2">
          <Input
            value={title}
            autoFocus
            placeholder="Movies & TV"
            disabled={off || busy}
            onChange={(e) => setTitle(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                if (title.trim()) void create();
              }
            }}
            className="flex-1"
          />
          <button
            type="button"
            disabled={off || busy || !title.trim()}
            onClick={() => void create()}
            className="rounded-control px-3 py-2 text-sm font-medium text-primary hover:bg-primary-soft disabled:opacity-60"
          >
            {busy ? "Creating…" : "Create"}
          </button>
        </div>
      )}
      {choice.kind === "missing" && !creating && (
        <p className="text-sm text-danger">
          This folder was removed. Pick another one or keep the files inside
          this app.
        </p>
      )}
      {error && <p className="text-sm text-danger">{error}</p>}
    </div>
  );
}
