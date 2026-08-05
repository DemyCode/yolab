import { useEffect, useState } from "react";
import {
  Check,
  ChevronDown,
  Copy,
  ExternalLink,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { Sheet, ConfirmDialog } from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Spinner } from "@/components/ui/feedback";
import { api, streamEvents } from "@/lib/api";
import { appDisplayName, appState, appUrl, catalogEntry } from "@/lib/apps";
import { taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp, PodInfo } from "@/types/apps";

function CopyRow({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center gap-3 rounded-xl bg-surface-2 px-3.5 py-3">
      <div className="min-w-0 flex-1">
        <div className="text-xs text-fg-muted">{label}</div>
        <div className="truncate font-mono text-sm text-fg">{value}</div>
      </div>
      <button
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(value);
            setCopied(true);
            setTimeout(() => setCopied(false), 1600);
          } catch {
            /* clipboard blocked outside a secure context; value is selectable */
          }
        }}
        className="rounded-lg p-2 text-fg-muted hover:bg-surface-3 hover:text-fg"
        aria-label={`Copy ${label}`}
      >
        {copied ? (
          <Check className="h-4 w-4 text-success" />
        ) : (
          <Copy className="h-4 w-4" />
        )}
      </button>
    </div>
  );
}

/**
 * The operator view, behind a disclosure.
 *
 * Pods and container logs are genuinely useful when something is broken, and
 * genuinely meaningless the rest of the time. Keeping them one tap away rather
 * than on the front page is the whole difference between a product and a
 * console — nothing is taken away, it just stops being the first thing anyone
 * sees.
 */
function Advanced({ app }: { app: AppInfo }) {
  const [open, setOpen] = useState(false);
  // `null` means "not fetched yet", which doubles as the loading state — a
  // separate `busy` flag would have to be set synchronously inside the effect.
  const [pods, setPods] = useState<PodInfo[] | null>(null);
  const [logs, setLogs] = useState<{ pod: string; text: string } | null>(null);

  useEffect(() => {
    if (!open || pods) return;
    let cancelled = false;
    void api
      .get<PodInfo[]>(`/api/apps/${app.instance_name}/pods`)
      .then((p) => {
        if (!cancelled) setPods(p);
      })
      .catch(() => {
        if (!cancelled) setPods([]);
      });
    return () => {
      cancelled = true;
    };
  }, [open, pods, app.instance_name]);

  return (
    <div className="mt-4 border-t border-border pt-4">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center justify-between text-sm font-medium text-fg-muted hover:text-fg"
        aria-expanded={open}
      >
        Technical details
        <ChevronDown
          className={cn("h-4 w-4 transition-transform", open && "rotate-180")}
        />
      </button>

      {open && (
        <div className="mt-3 space-y-3">
          {!pods ? (
            <div className="flex justify-center py-4">
              <Spinner />
            </div>
          ) : pods && pods.length > 0 ? (
            <div className="space-y-1.5">
              {pods.map((pod) => (
                <div
                  key={pod.name}
                  className="flex items-center gap-2 rounded-xl bg-surface-2 px-3 py-2"
                >
                  <span
                    className={cn(
                      "h-2 w-2 shrink-0 rounded-full",
                      pod.ready ? "bg-success" : "bg-warning",
                    )}
                  />
                  <span className="min-w-0 flex-1 truncate font-mono text-xs text-fg-muted">
                    {pod.name}
                  </span>
                  <span className="shrink-0 text-xs text-fg-subtle">
                    {pod.phase}
                  </span>
                  <button
                    onClick={async () => {
                      setLogs({ pod: pod.name, text: "" });
                      try {
                        const text = await api.getText(
                          `/api/apps/${app.instance_name}/logs/${pod.name}`,
                        );
                        setLogs({ pod: pod.name, text });
                      } catch (e) {
                        setLogs({
                          pod: pod.name,
                          text: e instanceof Error ? e.message : "No logs",
                        });
                      }
                    }}
                    className="shrink-0 rounded-lg px-2 py-1 text-xs text-primary hover:bg-surface-3"
                  >
                    Logs
                  </button>
                </div>
              ))}
            </div>
          ) : (
            <p className="text-sm text-fg-muted">Nothing running right now.</p>
          )}

          {logs && (
            <pre className="max-h-64 overflow-auto rounded-xl bg-surface-3 p-3 font-mono text-xs leading-relaxed text-fg-muted">
              {logs.text || "…"}
            </pre>
          )}

          {Object.keys(app.config ?? {}).length > 0 && (
            <div className="rounded-xl bg-surface-2 p-3">
              <div className="mb-2 text-xs font-medium text-fg-muted">
                Settings this app was installed with
              </div>
              <dl className="space-y-1">
                {Object.entries(app.config).map(([k, v]) => (
                  <div key={k} className="flex gap-2 text-xs">
                    <dt className="text-fg-subtle">{k}</dt>
                    <dd className="min-w-0 flex-1 truncate font-mono text-fg-muted">
                      {typeof v === "string" ? v : JSON.stringify(v)}
                    </dd>
                  </div>
                ))}
              </dl>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function AppDetailSheet({
  app,
  catalog,
  tunnelDomain,
  onClose,
  onChanged,
}: {
  app: AppInfo | null;
  catalog: CatalogApp[];
  tunnelDomain: string;
  onClose: () => void;
  onChanged: () => void;
}) {
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [working, setWorking] = useState<null | "update" | "remove">(null);
  const [error, setError] = useState<string | null>(null);

  if (!app) return null;

  const entry = catalogEntry(app, catalog);
  const name = appDisplayName(app, catalog);
  const url = appUrl(app, tunnelDomain);
  const state = appState(app);
  const visibleOutputs = (app.outputs ?? []).filter(
    (o) => o.type !== "hidden" && o.value,
  );

  async function remove() {
    if (!app) return;
    setWorking("remove");
    setError(null);
    try {
      // Uninstall streams progress; we only need it to finish, and the list
      // refresh below is what the user actually sees.
      await api.del(`/api/apps/${app.instance_name}`);
      setConfirmRemove(false);
      onChanged();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the app");
    } finally {
      setWorking(null);
    }
  }

  async function update() {
    if (!app) return;
    setWorking("update");
    setError(null);
    // Update streams Helm's output; we only care whether it reached [DONE].
    const result = await streamEvents(
      `/api/apps/${app.instance_name}/update`,
      { method: "POST" },
      () => {},
    );
    if (!result.ok) setError(result.error ?? "Could not update the app");
    onChanged();
    setWorking(null);
  }

  return (
    <>
      <Sheet
        open
        onClose={onClose}
        title={
          <span className="flex items-center gap-3">
            <span className="text-2xl" aria-hidden>
              {entry?.icon ?? "📦"}
            </span>
            {name}
          </span>
        }
        subtitle={
          entry
            ? taglineFor({ id: entry.id, description: entry.description })
            : undefined
        }
        footer={
          url && state === "ready" ? (
            <a
              href={url}
              target="_blank"
              rel="noopener noreferrer"
              className="flex h-11 w-full items-center justify-center gap-2 rounded-xl bg-primary text-sm font-medium text-primary-fg transition active:scale-[0.97]"
            >
              Open {name}
              <ExternalLink className="h-4 w-4" />
            </a>
          ) : (
            <Button full disabled>
              {state === "starting" ? "Starting up…" : "Not available"}
            </Button>
          )
        }
      >
        {state === "starting" && (
          <p className="mb-4 rounded-xl bg-primary-soft px-3.5 py-3 text-sm text-fg-muted">
            This app is still starting. It usually takes a minute or two the
            first time.
          </p>
        )}

        {error && (
          <p className="mb-4 rounded-xl bg-danger-soft px-3.5 py-3 text-sm text-danger">
            {error}
          </p>
        )}

        <div className="space-y-2">
          {url && <CopyRow label="Address" value={url} />}
          {visibleOutputs.map((o) => (
            <CopyRow key={o.key} label={o.label} value={o.value} />
          ))}
        </div>

        {entry && (
          <div className="mt-4 flex items-center gap-2">
            <Badge variant="neutral">version {entry.chart_version}</Badge>
            {entry.repo !== "official" && (
              // Charts from a repo the user added can create arbitrary cluster
              // objects. Saying where this one came from is the minimum.
              <Badge variant="warning">from {entry.repo}</Badge>
            )}
          </div>
        )}

        <div className="mt-5 flex flex-col gap-2 sm:flex-row">
          <Button
            variant="secondary"
            onClick={() => void update()}
            loading={working === "update"}
            className="flex-1"
          >
            <RefreshCw className="h-4 w-4" />
            Check for updates
          </Button>
          <Button
            variant="quiet"
            onClick={() => setConfirmRemove(true)}
            className="flex-1"
          >
            <Trash2 className="h-4 w-4" />
            Remove
          </Button>
        </div>

        <Advanced app={app} />
      </Sheet>

      <ConfirmDialog
        open={confirmRemove}
        onClose={() => setConfirmRemove(false)}
        onConfirm={() => void remove()}
        title={`Remove ${name}?`}
        destructive
        confirmLabel="Remove it"
        busy={working === "remove"}
        // Naming the consequence, not asking "are you sure". Everyone is sure
        // until they find out what it does.
        body={
          <>
            This deletes {name} and everything stored in it — files, settings
            and history. Backups you have already taken are kept, so this can be
            undone from a backup, but nothing added since the last one will
            survive.
          </>
        }
      />
    </>
  );
}
