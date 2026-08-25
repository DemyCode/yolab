import { useState } from "react";
import { Plus, RefreshCw, Trash2, Library, AlertTriangle, FileCode2 } from "lucide-react";
import { Link } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";

interface ChartRepo {
  name: string;
  url: string;
  /** False for the official catalog, which must not be removable. */
  removable: boolean;
}

/** What `POST /api/apps/repos/sync` reports back, per repo. */
type SyncResult = Record<string, { ok: boolean; charts?: number; error?: string }>;

/**
 * Where apps come from.
 *
 * local-api has had add/remove/sync for chart repositories the whole time —
 * validated, persisted to a ConfigMap, re-synced on a timer. Nothing in the UI
 * ever called any of it, so in practice YoLab shipped with exactly one source
 * of apps and no way to say otherwise. This is that missing screen; the
 * endpoints are unchanged.
 *
 * Deliberately at the bottom of the catalog rather than in Settings: the
 * question "why isn't the app I want in here?" is asked while looking at the
 * list it is missing from.
 */
export function AppSources({ onChanged }: { onChanged?: () => void }) {
  const repos = useResource<ChartRepo[]>("app-repos", () =>
    api.get<ChartRepo[]>("/api/apps/repos"),
  );
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [syncNote, setSyncNote] = useState<string | null>(null);

  async function add() {
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/apps/repos", { name: name.trim(), url: url.trim() });
      setName("");
      setUrl("");
      setAdding(false);
      await repos.refresh();
      onChanged?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not add that source");
    } finally {
      setBusy(false);
    }
  }

  async function remove(repo: ChartRepo) {
    setBusy(true);
    setError(null);
    try {
      await api.del(`/api/apps/repos/${encodeURIComponent(repo.name)}`);
      await repos.refresh();
      onChanged?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove that source");
    } finally {
      setBusy(false);
    }
  }

  async function sync() {
    setBusy(true);
    setError(null);
    setSyncNote(null);
    try {
      const res = await api.post<SyncResult>("/api/apps/repos/sync");
      // Report per source. A single "synced!" would hide the case that matters:
      // one source is broken and the app someone is looking for is in that one.
      const failed = Object.entries(res).filter(([, r]) => !r.ok);
      if (failed.length > 0) {
        setError(
          failed
            .map(([n, r]) => `${n}: ${r.error ?? "could not be read"}`)
            .join(" · "),
        );
      }
      const ok = Object.entries(res).filter(([, r]) => r.ok);
      if (ok.length > 0) {
        const total = ok.reduce((n, [, r]) => n + (r.charts ?? 0), 0);
        setSyncNote(`${total} app${total === 1 ? "" : "s"} available`);
      }
      onChanged?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not refresh");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="mt-10 rounded-card border border-border bg-surface p-4">
      <div className="flex flex-wrap items-center gap-3">
        <Library className="h-4 w-4 shrink-0 text-fg-muted" />
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-semibold text-fg">Where apps come from</h2>
          <p className="mt-0.5 text-sm text-fg-muted">
            YoLab ships one catalog. Add another and its apps appear above,
            alongside the rest.
          </p>
        </div>
        <div className="flex shrink-0 gap-2">
          <Button size="sm" variant="secondary" onClick={sync} disabled={busy}>
            <RefreshCw className={busy ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />
            Refresh
          </Button>
          <Button size="sm" onClick={() => setAdding((v) => !v)} disabled={busy}>
            <Plus className="h-3.5 w-3.5" />
            Add
          </Button>
        </div>
      </div>

      {adding && (
        <>
          {/* charts.rs says this plainly and the UI has to as well: a chart can
              declare any cluster object, so adding a source hands its publisher
              the ability to do anything here. It is not apt-get. */}
          <p className="mt-4 flex items-start gap-2 rounded-md border border-warning-soft bg-warning-soft p-3 text-sm text-warning">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
            <span>
              Apps from another source run with the same access as the ones YoLab
              ships. Only add a source you would trust with the whole machine.
            </span>
          </p>
          <div className="mt-3 flex flex-col gap-2 sm:flex-row">
          <Input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Name — e.g. community"
            aria-label="Source name"
            className="sm:w-56"
          />
          <Input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://…"
            aria-label="Source URL"
            className="flex-1"
          />
            <Button onClick={add} disabled={busy || !name.trim() || !url.trim()}>
              {busy ? "Adding…" : "Add source"}
            </Button>
          </div>
        </>
      )}

      {error && (
        <p className="mt-3 flex items-start gap-2 text-sm text-danger">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
          <span>{error}</span>
        </p>
      )}
      {syncNote && !error && (
        <p className="mt-3 text-sm text-fg-muted">{syncNote}</p>
      )}

      <ul className="mt-4 divide-y divide-border">
        {repos.loading && (
          <li className="py-2 text-sm text-fg-muted">Loading…</li>
        )}
        {repos.data?.map((r) => (
          <li key={r.name} className="flex items-center gap-3 py-2">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span className="font-medium text-fg">{r.name}</span>
                {!r.removable && (
                  <span className="rounded-full bg-surface-2 px-2 py-0.5 text-xs text-fg-muted">
                    Built in
                  </span>
                )}
              </div>
              <p className="truncate text-xs text-fg-subtle">{r.url}</p>
            </div>
            {r.removable && (
              <button
                type="button"
                onClick={() => remove(r)}
                disabled={busy}
                aria-label={`Remove ${r.name}`}
                className="shrink-0 rounded-md p-1.5 text-fg-subtle transition-colors hover:bg-surface-2 hover:text-danger disabled:opacity-50"
              >
                <Trash2 className="h-4 w-4" />
              </button>
            )}
          </li>
        ))}
      </ul>

      <div className="mt-4 border-t border-border pt-4">
        <Link
          to="/add/custom"
          className="flex items-center gap-2 text-sm text-fg-muted transition-colors hover:text-fg"
        >
          <FileCode2 className="h-4 w-4 shrink-0" />
          <span>
            Not here at all? <span className="text-fg">Add your own app</span>{" "}
            from Kubernetes YAML.
          </span>
        </Link>
      </div>
    </section>
  );
}
