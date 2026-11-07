import { useEffect, useState, type KeyboardEvent, type ReactNode } from "react";
import type { WidgetProps } from "@rjsf/utils";
import {
  Check,
  ChevronRight,
  Eye,
  EyeOff,
  Folder,
  RefreshCw,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input, Toggle } from "@/components/ui/input";
import { ApiError } from "@/lib/api";
import { generateSecret } from "@/lib/format";
import {
  ROOT_FOLDER,
  cleanFolder,
  folderTrail,
  isRootFolder,
  joinFolder,
  listFolders,
  rankFolders,
  shownFolder,
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
  const picked = v === "" ? null : isRootFolder(v) ? "" : cleanFolder(v);
  const [at, setAt] = useState(() =>
    picked ? splitFolder(picked).parent : "",
  );
  const [filter, setFilter] = useState("");
  const [listing, setListing] = useState<{
    at: string;
    folders: string[];
    truncated: boolean;
    failure: string | null;
  } | null>(null);
  const [active, setActive] = useState(0);
  const locked = disabled || readonly;

  useEffect(() => {
    let live = true;
    listFolders(at).then(
      (l) => {
        if (live)
          setListing({
            at,
            folders: l.folders,
            truncated: l.truncated,
            failure: null,
          });
      },
      (e: unknown) => {
        if (!live) return;
        const failure =
          e instanceof ApiError && e.status === 404
            ? `There is no folder ${shownFolder(at)} in your file tree.`
            : `The file tree could not be read: ${e instanceof Error ? e.message : String(e)}`;
        setListing({ at, folders: [], truncated: false, failure });
      },
    );
    return () => {
      live = false;
    };
  }, [at]);

  const known = listing?.at === at ? listing : null;
  const folders = known
    ? rankFolders(known.folders, filter, known.folders.length)
    : [];

  const go = (path: string) => {
    setAt(path);
    setFilter("");
    setActive(0);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" && folders.length > 0) {
      e.preventDefault();
      setActive((a) => (a + 1) % folders.length);
    } else if (e.key === "ArrowUp" && folders.length > 0) {
      e.preventDefault();
      setActive((a) => (a - 1 + folders.length) % folders.length);
    } else if (e.key === "Enter") {
      e.preventDefault();
      const chosen = folders[Math.min(active, folders.length - 1)];
      if (chosen) go(joinFolder(at, chosen));
    } else if (e.key === "Backspace" && filter === "" && at !== "") {
      e.preventDefault();
      go(splitFolder(at).parent);
    }
  };

  let body: ReactNode;
  if (!known) {
    body = <p className="px-3 py-2 text-sm text-fg-muted">Looking…</p>;
  } else if (known.failure) {
    body = <p className="px-3 py-2 text-sm text-danger">{known.failure}</p>;
  } else if (folders.length === 0) {
    body = (
      <p className="px-3 py-2 text-sm text-fg-muted">
        {filter
          ? `No folder in ${shownFolder(at)} matches “${filter}”.`
          : `${shownFolder(at)} has no folders inside it.`}
      </p>
    );
  } else {
    body = (
      <ul role="listbox" aria-label={`Folders in ${shownFolder(at)}`}>
        {folders.map((folder, i) => (
          <li
            key={folder}
            role="option"
            aria-selected={i === active}
            onMouseDown={(e) => {
              e.preventDefault();
              if (!locked) go(joinFolder(at, folder));
            }}
            onMouseEnter={() => setActive(i)}
            className={cn(
              "flex cursor-pointer items-center gap-2 px-3 py-2 font-mono text-sm text-fg",
              i === active && "bg-surface-2",
            )}
          >
            <Folder className="h-4 w-4 shrink-0 text-fg-muted" />
            <span className="min-w-0 flex-1 truncate">{folder}</span>
            <ChevronRight className="h-4 w-4 shrink-0 text-fg-subtle" />
          </li>
        ))}
      </ul>
    );
  }

  const here = shownFolder(at);
  const pickedHere = picked === at;

  return (
    <div className="space-y-2">
      <div className="overflow-hidden rounded-control border border-border bg-surface">
        <nav
          aria-label="Folder you are looking in"
          className="flex flex-wrap items-center gap-0.5 border-b border-border px-2 py-1.5 font-mono text-sm"
        >
          {folderTrail(at).map((step, i, all) => (
            <span key={step.path} className="inline-flex items-center gap-0.5">
              {i > 1 && <ChevronRight className="h-3.5 w-3.5 text-fg-subtle" />}
              <button
                type="button"
                disabled={locked || i === all.length - 1}
                onClick={() => go(step.path)}
                className={cn(
                  "rounded px-1.5 py-0.5",
                  i === all.length - 1
                    ? "font-medium text-fg"
                    : "text-fg-muted hover:bg-surface-2 hover:text-fg",
                )}
              >
                {step.name}
              </button>
            </span>
          ))}
        </nav>
        <div className="border-b border-border p-2">
          <Input
            id={id}
            value={filter}
            autoFocus={autofocus}
            autoComplete="off"
            spellCheck={false}
            placeholder={`Filter the folders in ${here}`}
            disabled={locked}
            onKeyDown={onKeyDown}
            onChange={(e) => {
              setFilter(e.target.value.replace(/\//g, ""));
              setActive(0);
            }}
            className="font-mono"
          />
        </div>
        <div className="max-h-64 overflow-auto py-1">{body}</div>
        {known?.truncated && !filter && (
          <p className="border-t border-border px-3 py-1.5 text-xs text-fg-muted">
            Only some folders are shown; type to find the others.
          </p>
        )}
        <div className="flex items-center justify-between gap-2 border-t border-border px-2 py-2">
          <span className="min-w-0 truncate text-sm">
            {picked === null ? (
              <span className="text-fg-muted">No folder picked yet.</span>
            ) : (
              <span className="inline-flex min-w-0 items-center gap-1 text-success">
                <Check className="h-3.5 w-3.5 shrink-0" />
                <span className="truncate font-mono">{shownFolder(picked)}</span>
              </span>
            )}
          </span>
          <Button
            type="button"
            size="sm"
            variant={pickedHere ? "secondary" : "primary"}
            disabled={locked || pickedHere || Boolean(known?.failure)}
            onClick={() => onChange(at === "" ? ROOT_FOLDER : shownFolder(at))}
            className="shrink-0 font-mono"
          >
            {pickedHere ? `Using ${here}` : `Use ${here}`}
          </Button>
        </div>
      </div>
    </div>
  );
}
