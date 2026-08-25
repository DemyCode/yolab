import { useEffect, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { ArrowLeft, Search, X } from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppCard } from "@/components/AppCard";
import { Input } from "@/components/ui/input";
import { Skeleton, EmptyState } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import { GROUPS, groupFor, groupLabel, taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp } from "@/types/apps";

type Installed = "any" | "installed" | "not-installed";

/**
 * Search, as distinct from Discover.
 *
 * Discover answers "show me what there is"; this answers "I know roughly what I want,
 * narrow it down". They want opposite layouts — browsing wants grouping and breathing
 * room, narrowing wants one flat ranked list and filters that stack — so trying to be
 * both is what made the old page a grid of 70 unknown names with a search box on top.
 *
 * The query lives in the URL so a search can be linked, shared and gone back to.
 */
export default function SearchPage() {
  const [params, setParams] = useSearchParams();
  const [query, setQuery] = useState(params.get("q") ?? "");
  const [groups, setGroups] = useState<Set<string>>(new Set());
  const [source, setSource] = useState<string>("any");
  const [installed, setInstalled] = useState<Installed>("any");

  const catalog = useResource<CatalogApp[]>("catalog", () =>
    api.get("/api/apps/catalog"),
  );
  const apps = useResource<AppInfo[]>("apps", () => api.get("/api/apps"));

  // Debounced so the address bar does not gain one entry per keystroke.
  useEffect(() => {
    const t = setTimeout(() => {
      const next = new URLSearchParams(params);
      if (query.trim()) next.set("q", query.trim());
      else next.delete("q");
      setParams(next, { replace: true });
    }, 300);
    return () => clearTimeout(t);
    // `params`/`setParams` are intentionally omitted: including them re-runs this
    // on the very change it just made.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query]);

  const installedCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const a of apps.data ?? []) {
      counts.set(a.app_id, (counts.get(a.app_id) ?? 0) + 1);
    }
    return counts;
  }, [apps.data]);

  /** Every source present in the catalog, so the filter lists only real options. */
  const sources = useMemo(() => {
    const s = new Set<string>();
    for (const a of catalog.data ?? []) s.add(a.repo);
    return [...s].sort();
  }, [catalog.data]);

  const results = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (catalog.data ?? [])
      .filter((a) => {
        if (groups.size > 0 && !groups.has(groupFor(a))) return false;
        if (source !== "any" && a.repo !== source) return false;
        const n = installedCounts.get(a.id) ?? 0;
        if (installed === "installed" && n === 0) return false;
        if (installed === "not-installed" && n > 0) return false;
        if (!q) return true;
        // The tagline is searched too, so "netflix" finds Jellyfin and "1password"
        // finds Vaultwarden — people search for the thing they already know.
        return `${a.name} ${a.id} ${taglineFor(a)} ${a.description}`
          .toLowerCase()
          .includes(q);
      })
      .sort((a, b) => {
        if (!q) return a.name.localeCompare(b.name);
        // A name match beats a match buried in a description: someone typing
        // "photo" means the app called Photoprism before one that mentions photos.
        const an = a.name.toLowerCase().startsWith(q) ? 0 : a.name.toLowerCase().includes(q) ? 1 : 2;
        const bn = b.name.toLowerCase().startsWith(q) ? 0 : b.name.toLowerCase().includes(q) ? 1 : 2;
        return an - bn || a.name.localeCompare(b.name);
      });
  }, [catalog.data, query, groups, source, installed, installedCounts]);

  const filtersOn = groups.size > 0 || source !== "any" || installed !== "any";

  function toggleGroup(id: string) {
    setGroups((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function clearFilters() {
    setGroups(new Set());
    setSource("any");
    setInstalled("any");
  }

  return (
    <Page wide title="Search apps" subtitle="Narrow it down until you find it.">
      <Link
        to="/add"
        className="mb-5 inline-flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
      >
        <ArrowLeft className="h-4 w-4" />
        Browse instead
      </Link>

      <div className="relative mb-4">
        <Search className="pointer-events-none absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-fg-subtle" />
        <Input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search — try 'photos' or 'netflix'"
          className="pl-10"
          type="search"
          autoFocus
          aria-label="Search apps"
        />
      </div>

      <div className="mb-5 space-y-3">
        <div className="flex flex-wrap gap-2">
          {GROUPS.map((g) => (
            <button
              key={g.id}
              onClick={() => toggleGroup(g.id)}
              aria-pressed={groups.has(g.id)}
              className={cn(
                "rounded-full px-3 py-1 text-sm transition-colors",
                groups.has(g.id)
                  ? "bg-primary text-primary-fg"
                  : "bg-surface-2 text-fg-muted hover:text-fg",
              )}
            >
              {g.label}
            </button>
          ))}
        </div>

        <div className="flex flex-wrap items-center gap-3 text-sm">
          <label className="flex items-center gap-2">
            <span className="text-fg-muted">Status</span>
            <select
              value={installed}
              onChange={(e) => setInstalled(e.target.value as Installed)}
              className="rounded-md border border-border bg-bg px-2 py-1 text-fg"
            >
              <option value="any">Any</option>
              <option value="not-installed">Not installed</option>
              <option value="installed">Installed</option>
            </select>
          </label>

          {sources.length > 1 && (
            <label className="flex items-center gap-2">
              <span className="text-fg-muted">Source</span>
              <select
                value={source}
                onChange={(e) => setSource(e.target.value)}
                className="rounded-md border border-border bg-bg px-2 py-1 text-fg"
              >
                <option value="any">Any</option>
                {sources.map((s) => (
                  <option key={s} value={s}>
                    {s === "official" ? "YoLab catalog" : s === "custom" ? "Your own" : s}
                  </option>
                ))}
              </select>
            </label>
          )}

          {filtersOn && (
            <button
              onClick={clearFilters}
              className="inline-flex items-center gap-1 text-fg-muted hover:text-fg"
            >
              <X className="h-3.5 w-3.5" />
              Clear filters
            </button>
          )}

          <span className="ml-auto text-fg-subtle">
            {catalog.loading
              ? ""
              : `${results.length} of ${catalog.data?.length ?? 0}`}
          </span>
        </div>
      </div>

      {catalog.loading ? (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {Array.from({ length: 9 }, (_, i) => (
            <Skeleton key={i} className="h-[5.5rem]" />
          ))}
        </div>
      ) : results.length === 0 ? (
        <EmptyState
          icon={<Search className="h-6 w-6" />}
          title="Nothing matches"
          body={
            filtersOn
              ? "No app matches both the search and the filters. Try clearing a filter."
              : `No app matches "${query}". Try a different word.`
          }
        />
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {results.map((app) => (
            <AppCard
              key={`${app.repo}/${app.id}`}
              app={app}
              count={installedCounts.get(app.id) ?? 0}
            />
          ))}
        </div>
      )}

      {/* Group headings are omitted on purpose: a ranked list is the point here, and
          re-grouping it would put the best match halfway down the page. */}
      {!catalog.loading && results.length > 0 && groups.size === 1 && (
        <p className="mt-4 text-xs text-fg-subtle">
          Showing {groupLabel([...groups][0])} only.
        </p>
      )}
    </Page>
  );
}
