import { useCallback, useEffect, useState } from "react";
import { ExternalLink, Eye, EyeOff, RefreshCw } from "lucide-react";
import { Spinner } from "@/components/ui/feedback";
import {
  CopyButton,
  IconButton,
  RowAction,
  Section,
} from "@/components/ui/list";
import { api } from "@/lib/api";
import {
  accessRows,
  outputState,
  stillWaiting,
  type AppLink,
} from "@/lib/apps";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { AppOutput, OutputsResponse } from "@/types/apps";

const WAITING_POLL_MS = 15_000;

function LinkRow({ link }: { link: AppLink }) {
  return (
    <div className="px-5 py-4">
      <div className="text-sm font-medium text-fg">{link.label}</div>
      <div className="mt-1 flex items-center gap-1">
        <code
          className="min-w-0 flex-1 truncate font-mono text-sm text-fg-muted"
          title={link.url}
        >
          {link.url}
        </code>
        <IconButton label={`Open ${link.label}`} href={link.url}>
          <ExternalLink className="h-4 w-4" />
        </IconButton>
        <CopyButton value={link.url} label={link.label} />
      </div>
    </div>
  );
}

function OutputValue({ output }: { output: AppOutput }) {
  const [revealed, setRevealed] = useState(false);
  const value = output.value ?? "";

  if (output.format === "multiline") {
    return (
      <div className="relative mt-2">
        <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all rounded-control bg-surface-2 p-3 pr-12 font-mono text-xs leading-relaxed text-fg">
          {value}
        </pre>
        <div className="absolute right-1.5 top-1.5">
          <CopyButton value={value} label={output.title} />
        </div>
      </div>
    );
  }

  const hidden = output.format === "secret" && !revealed;
  return (
    <div className="mt-1 flex items-center gap-1">
      <code
        className={cn(
          "min-w-0 flex-1 truncate font-mono text-sm text-fg-muted",
          hidden && "tracking-widest",
        )}
        title={hidden ? undefined : value}
      >
        {hidden ? "••••••••••••" : value}
      </code>
      {output.format === "secret" && (
        <IconButton
          label={revealed ? `Hide ${output.title}` : `Show ${output.title}`}
          onClick={() => setRevealed((r) => !r)}
        >
          {revealed ? (
            <EyeOff className="h-4 w-4" />
          ) : (
            <Eye className="h-4 w-4" />
          )}
        </IconButton>
      )}
      <CopyButton value={value} label={output.title} />
    </div>
  );
}

function OutputRow({ output }: { output: AppOutput }) {
  const state = outputState(output);
  return (
    <div className="px-5 py-4">
      <div className="flex items-baseline justify-between gap-3">
        <span className="text-sm font-medium text-fg">{output.title}</span>
        <span className="shrink-0 text-xs text-fg-subtle">
          {state === "ready" &&
            !output.from_config &&
            output.found_at &&
            `Reported ${relativeTime(output.found_at)}`}
        </span>
      </div>
      {state === "ready" && <OutputValue output={output} />}
      {state === "waiting" && (
        <p className="mt-1.5 flex items-center gap-2 text-sm text-fg-muted">
          <Spinner className="h-3 w-3" />
          Waiting for the app to report this
        </p>
      )}
      {state === "unset" && (
        <p className="mt-1 text-sm text-fg-subtle">Not set</p>
      )}
    </div>
  );
}

export function AppAccess({
  instanceName,
  appReady,
  links,
}: {
  instanceName: string;
  appReady: boolean;
  links: AppLink[];
}) {
  const [outputs, setOutputs] = useState<AppOutput[] | null>(null);
  const [checking, setChecking] = useState(false);
  const [failed, setFailed] = useState(false);

  const load = useCallback(async () => {
    try {
      const r = await api.get<OutputsResponse>(
        `/api/apps/${instanceName}/outputs`,
      );
      setOutputs(r.outputs);
      setFailed(false);
    } catch {
      setFailed(true);
    }
  }, [instanceName]);

  const checkNow = useCallback(async () => {
    setChecking(true);
    try {
      const r = await api.post<OutputsResponse>(
        `/api/apps/${instanceName}/scan-outputs`,
      );
      setOutputs(r.outputs);
      setFailed(false);
    } catch {
      setFailed(true);
    } finally {
      setChecking(false);
    }
  }, [instanceName]);

  useEffect(() => {
    void load();
  }, [load]);

  const waiting = outputs !== null && stillWaiting(outputs);
  useEffect(() => {
    if (!waiting || !appReady) return;
    const id = window.setInterval(() => void load(), WAITING_POLL_MS);
    return () => clearInterval(id);
  }, [waiting, appReady, load]);

  const rows = accessRows(outputs ?? []);
  const unreadable = failed && outputs === null;
  const loading = outputs === null && !failed;
  if (links.length === 0 && rows.length === 0 && !unreadable && !loading) {
    return null;
  }

  return (
    <Section
      title="Access"
      action={
        (waiting || unreadable) && (
          <RowAction onClick={() => void checkNow()} disabled={checking}>
            <RefreshCw
              className={cn("h-3.5 w-3.5", checking && "animate-spin")}
            />
            {checking ? "Checking…" : "Check again"}
          </RowAction>
        )
      }
    >
      {links.map((link) => (
        <LinkRow key={link.url} link={link} />
      ))}
      {rows.map((o) => (
        <OutputRow key={o.key} output={o} />
      ))}
      {loading && links.length === 0 && (
        <div className="flex justify-center px-5 py-4">
          <Spinner className="h-4 w-4" />
        </div>
      )}
      {unreadable && (
        <p className="px-5 py-4 text-sm text-fg-muted">
          Passwords and other details could not be loaded right now.
        </p>
      )}
      {waiting && (
        <p className="px-5 py-3 text-xs text-fg-subtle">
          YoLab reads the app&rsquo;s logs every minute and keeps the last value
          it finds, so these appear on their own once the app prints them.
        </p>
      )}
    </Section>
  );
}
