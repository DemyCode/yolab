import { useEffect, useState, type KeyboardEvent, type ReactNode } from "react";
import type { WidgetProps } from "@rjsf/utils";
import { Check, Eye, EyeOff, Folder, RefreshCw } from "lucide-react";
import { Input, Toggle } from "@/components/ui/input";
import { ApiError } from "@/lib/api";
import { generateSecret } from "@/lib/format";
import {
  cleanFolder,
  folderIn,
  joinFolder,
  listFolders,
  rankFolders,
  splitFolder,
} from "@/lib/treeFolder";
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

export function TreeFolderWidget(props: WidgetProps) {
  const { value, onChange, disabled, readonly, id, autofocus } = props;
  const v = typeof value === "string" ? value : "";
  const { parent, name } = splitFolder(v);
  const [listing, setListing] = useState<{
    parent: string;
    folders: string[];
    failure: string | null;
  } | null>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);

  useEffect(() => {
    let live = true;
    listFolders(parent).then(
      (l) => {
        if (live) setListing({ parent, folders: l.folders, failure: null });
      },
      (e: unknown) => {
        if (!live) return;
        const failure =
          e instanceof ApiError && e.status === 404
            ? `There is no folder ${parent} in your file tree.`
            : `The file tree could not be read: ${e instanceof Error ? e.message : String(e)}`;
        setListing({ parent, folders: [], failure });
      },
    );
    return () => {
      live = false;
    };
  }, [parent]);

  const known = listing?.parent === parent ? listing : null;
  const suggestions = known ? rankFolders(known.folders, name) : [];
  const exists = known !== null && folderIn(known.folders, name);
  const shown = open && suggestions.length > 0 && !(exists && suggestions.length === 1);

  const pick = (folder: string) => {
    onChange(joinFolder(parent, folder));
    setActive(0);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (!shown) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((a) => (a + 1) % suggestions.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => (a - 1 + suggestions.length) % suggestions.length);
    } else if (e.key === "Enter" || e.key === "Tab") {
      const chosen = suggestions[Math.min(active, suggestions.length - 1)];
      if (chosen && chosen !== name) {
        e.preventDefault();
        pick(chosen);
      }
    } else if (e.key === "Escape") {
      setOpen(false);
    }
  };

  let status: ReactNode;
  if (!v) {
    status = (
      <span className="text-fg-muted">
        Start typing; add / to look inside a folder.
      </span>
    );
  } else if (!known) {
    status = <span className="text-fg-muted">Looking in your file tree…</span>;
  } else if (known.failure) {
    status = <span className="text-danger">{known.failure}</span>;
  } else if (exists) {
    status = (
      <span className="inline-flex items-center gap-1 text-success">
        <Check className="h-3.5 w-3.5" />
        {cleanFolder(v)}
      </span>
    );
  } else {
    status = (
      <span className="text-danger">
        No folder named {name || "…"} {parent ? `in ${parent}` : "at the top of your file tree"}.
      </span>
    );
  }

  return (
    <div className="relative space-y-1.5">
      <Input
        id={id}
        value={v}
        autoFocus={autofocus}
        autoComplete="off"
        spellCheck={false}
        role="combobox"
        aria-expanded={shown}
        aria-autocomplete="list"
        disabled={disabled || readonly}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onKeyDown={onKeyDown}
        onChange={(e) => {
          onChange(e.target.value.replace(/^\/+/, "").replace(/\/{2,}/g, "/"));
          setActive(0);
          setOpen(true);
        }}
        className="font-mono"
      />
      {shown && (
        <ul
          role="listbox"
          className="absolute inset-x-0 top-12 z-20 max-h-64 overflow-auto rounded-control border border-border bg-surface py-1 shadow-pop"
        >
          {suggestions.map((folder, i) => (
            <li
              key={folder}
              role="option"
              aria-selected={i === active}
              onMouseDown={(e) => {
                e.preventDefault();
                pick(folder);
              }}
              onMouseEnter={() => setActive(i)}
              className={cn(
                "flex cursor-pointer items-center gap-2 px-3 py-1.5 font-mono text-sm text-fg",
                i === active && "bg-surface-2",
              )}
            >
              <Folder className="h-4 w-4 shrink-0 text-fg-muted" />
              <span className="truncate">{joinFolder(parent, folder)}</span>
            </li>
          ))}
        </ul>
      )}
      <p className="truncate text-sm">{status}</p>
    </div>
  );
}
