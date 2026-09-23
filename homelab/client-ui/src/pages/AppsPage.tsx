import { useMemo, useState } from "react";
import { Package, Search, SlidersHorizontal, X } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Input } from "@/components/ui/input";
import { AppCard } from "@/components/AppCard";
import { Skeleton, EmptyState } from "@/components/ui/feedback";
import { useApi } from "@/lib/useResource";
import { installedByChart } from "@/lib/apps";
import { GROUPS, groupFor, groupLabel, taglineFor } from "@/catalog/meta";
import { AppSources } from "@/components/AppSources";
import { AddFromBackupButton } from "@/components/AddFromBackup";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp } from "@/types/apps";

type Installed = "any" | "installed" | "not-installed";

export function AppsPage() {
  const [query, setQuery] = useState("");
  const [activeGroup, setActiveGroup] = useState<string | null>(null);
  const [filtersOpen, setFiltersOpen] = useState(false);
  const [source, setSource] = useState("any");
  const [installed, setInstalled] = useState<Installed>("any");

  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const apps = useApi<AppInfo[]>("apps", "/api/apps");

  const installedCounts = useMemo(
    () => installedByChart(apps.data),
    [apps.data],
  );

  const sources = useMemo(() => {
    const s = new Set<string>();
    for (const a of catalog.data ?? []) s.add(a.repo);
    return [...s].sort();
  }, [catalog.data]);

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (catalog.data ?? [])
      .filter((a) => {
        if (activeGroup && groupFor(a) !== activeGroup) return false;
        if (source !== "any" && a.repo !== source) return false;
        const n = installedCounts.get(a.id) ?? 0;
        if (installed === "installed" && n === 0) return false;
        if (installed === "not-installed" && n > 0) return false;
        if (!q) return true;
        return `${a.name} ${a.id} ${taglineFor(a)} ${a.description}`
          .toLowerCase()
          .includes(q);
      })
      .sort((a, b) => {
        if (!q) return a.name.localeCompare(b.name);
        const an = a.name.toLowerCase().startsWith(q)
          ? 0
          : a.name.toLowerCase().includes(q)
            ? 1
            : 2;
        const bn = b.name.toLowerCase().startsWith(q)
          ? 0
          : b.name.toLowerCase().includes(q)
            ? 1
            : 2;
        return an - bn || a.name.localeCompare(b.name);
      });
  }, [catalog.data, query, activeGroup, source, installed, installedCounts]);

  const browsing = !query.trim() && source === "any" && installed === "any";
  const filtersOn = source !== "any" || installed !== "any";

  const grouped = useMemo(() => {
    if (!browsing) return [];
    const byGroup = new Map<string, CatalogApp[]>();
    for (const app of matches) {
      const g = groupFor(app);
      const list = byGroup.get(g) ?? [];
      list.push(app);
      byGroup.set(g, list);
    }
    const order = GROUPS.map((g) => g.id);
    return [...byGroup.entries()].sort(
      (a, b) => order.indexOf(a[0]) - order.indexOf(b[0]),
    );
  }, [matches, browsing]);

  function clearFilters() {
    setSource("any");
    setInstalled("any");
  }

  const showEmpty = !catalog.loading && matches.length === 0;

  return (
    <Page
      wide
      title="Apps"
      subtitle="Everything here runs at home, on your own machines."
      action={<AddFromBackupButton />}
    >
      {}
      <div className="mb-4 flex gap-2">
        <div className="relative flex-1">
          <Search className="pointer-events-none absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-fg-subtle" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search — try 'photos' or 'netflix'"
            className="pl-10"
            type="search"
            aria-label="Search apps"
          />
        </div>
        <button
          onClick={() => setFiltersOpen((o) => !o)}
          aria-expanded={filtersOpen}
          className={cn(
            "inline-flex shrink-0 items-center gap-1.5 rounded-md border px-3 text-sm transition-colors",
            filtersOn
              ? "border-primary/30 bg-primary-soft text-primary"
              : "border-border text-fg-muted hover:border-border-strong hover:text-fg",
          )}
        >
          <SlidersHorizontal className="h-3.5 w-3.5" />
          Filters
        </button>
      </div>

      {}
      <div className="-mx-5 mb-4 flex gap-2 overflow-x-auto px-5 pb-1 md:mx-0 md:px-0">
        <button
          onClick={() => setActiveGroup(null)}
          className={cn(
            "shrink-0 rounded-full px-3.5 py-1.5 text-sm transition-colors",
            activeGroup === null
              ? "bg-primary text-primary-fg"
              : "bg-surface-2 text-fg-muted hover:text-fg",
          )}
        >
          Everything
        </button>
        {GROUPS.map((g) => (
          <button
            key={g.id}
            onClick={() => setActiveGroup(g.id === activeGroup ? null : g.id)}
            className={cn(
              "shrink-0 rounded-full px-3.5 py-1.5 text-sm transition-colors",
              activeGroup === g.id
                ? "bg-primary text-primary-fg"
                : "bg-surface-2 text-fg-muted hover:text-fg",
            )}
          >
            {g.label}
          </button>
        ))}
      </div>

      {filtersOpen && (
        <div className="mb-5 flex flex-wrap items-center gap-3 rounded-card border border-border bg-surface p-3 text-sm">
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
                    {s === "official"
                      ? "YoLab catalog"
                      : s === "custom"
                        ? "Your own"
                        : s}
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
              Clear
            </button>
          )}

          <span className="ml-auto text-fg-subtle">
            {catalog.loading
              ? ""
              : `${matches.length} of ${catalog.data?.length ?? 0}`}
          </span>
        </div>
      )}

      {catalog.loading ? (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {Array.from({ length: 9 }, (_, i) => (
            <Skeleton key={i} className="h-[5.5rem]" />
          ))}
        </div>
      ) : showEmpty ? (
        <EmptyState
          icon={<Package className="h-6 w-6" />}
          title={
            query.trim() || filtersOn || activeGroup
              ? "Nothing matches that"
              : "No apps yet"
          }
          body={
            query.trim() || filtersOn || activeGroup
              ? "No app matches the search and filters. Try a different word, or clear a filter above."
              : "Nothing is available to install yet. Add a source below, or sync the catalog."
          }
        />
      ) : browsing ? (
        <div className="space-y-8">
          {grouped.map(([groupId, groupApps]) => (
            <section key={groupId}>
              <h2 className="mb-3 text-sm font-semibold text-fg-muted">
                {groupLabel(groupId)}
              </h2>
              <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                {groupApps.map((app) => (
                  <AppCard
                    key={`${app.repo}/${app.id}`}
                    app={app}
                    count={installedCounts.get(app.id) ?? 0}
                  />
                ))}
              </div>
            </section>
          ))}
        </div>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {matches.map((app) => (
            <AppCard
              key={`${app.repo}/${app.id}`}
              app={app}
              count={installedCounts.get(app.id) ?? 0}
            />
          ))}
        </div>
      )}

      {}
      <AppSources onChanged={() => void catalog.refresh()} />
    </Page>
  );
}
