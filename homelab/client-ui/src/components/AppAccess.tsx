import { useCallback, useEffect, useState } from "react";
import { Check, Copy, Eye, EyeOff, KeyRound, RefreshCw } from "lucide-react";
import { Card } from "@/components/ui/card";
import { Spinner } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { accessRows, outputState, stillWaiting } from "@/lib/apps";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { AppOutput, OutputsResponse } from "@/types/apps";

const WAITING_POLL_MS = 15_000;

function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(value);
          setCopied(true);
          setTimeout(() => setCopied(false), 1600);
          // eslint-disable-next-line no-empty
        } catch {}
      }}
      className="shrink-0 rounded-lg p-2 text-fg-muted transition-colors hover:bg-surface-2 hover:text-fg"
      aria-label={`Copy ${label}`}
    >
      {copied ? (
        <Check className="h-4 w-4 text-success" />
      ) : (
        <Copy className="h-4 w-4" />
      )}
    </button>
  );
}

function OutputValue({ output }: { output: AppOutput }) {
  const [revealed, setRevealed] = useState(false);
  const value = output.value ?? "";

  if (output.format === "multiline") {
    return (
      <div className="relative mt-2">
        <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all rounded-lg bg-surface-2 p-3 pr-12 font-mono text-xs leading-relaxed text-fg">
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
          "min-w-0 flex-1 truncate font-mono text-sm text-fg",
          hidden && "tracking-widest text-fg-muted",
        )}
        title={hidden ? undefined : value}
      >
        {hidden ? "••••••••••••" : value}
      </code>
      {output.format === "secret" && (
        <button
          type="button"
          onClick={() => setRevealed((r) => !r)}
          className="shrink-0 rounded-lg p-2 text-fg-muted transition-colors hover:bg-surface-2 hover:text-fg"
          aria-label={revealed ? `Hide ${output.title}` : `Show ${output.title}`}
        >
          {revealed ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
        </button>
      )}
      {output.format === "uri" ? (
        <a
          href={value}
          target="_blank"
          rel="noopener noreferrer"
          className="shrink-0 rounded-lg px-2 py-1.5 text-sm text-primary hover:bg-primary-soft"
        >
          Open
        </a>
      ) : null}
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
          {state === "ready" && output.from_config && "Chosen at install"}
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
}: {
  instanceName: string;
  appReady: boolean;
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
  if (outputs === null && !failed) return null;
  if (outputs !== null && rows.length === 0) return null;

  return (
    <Card className="mb-4 p-0">
      <div className="flex items-center justify-between gap-3 border-b border-border px-5 py-3.5">
        <div className="flex items-center gap-2">
          <KeyRound className="h-4 w-4 text-fg-muted" />
          <h2 className="text-sm font-semibold text-fg">Access</h2>
        </div>
        <button
          type="button"
          onClick={() => void checkNow()}
          disabled={checking}
          className="inline-flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-xs text-fg-muted transition-colors hover:bg-surface-2 hover:text-fg disabled:opacity-60"
        >
          <RefreshCw className={cn("h-3.5 w-3.5", checking && "animate-spin")} />
          {checking ? "Checking…" : "Check again"}
        </button>
      </div>
      {failed && outputs === null ? (
        <p className="px-5 py-4 text-sm text-fg-muted">
          These details could not be loaded right now.
        </p>
      ) : (
        <div className="divide-y divide-border">
          {rows.map((o) => (
            <OutputRow key={o.key} output={o} />
          ))}
        </div>
      )}
      {waiting && (
        <p className="border-t border-border px-5 py-3 text-xs text-fg-subtle">
          YoLab reads the app&rsquo;s logs every minute and keeps the last value
          it finds, so these appear on their own once the app prints them.
        </p>
      )}
    </Card>
  );
}
