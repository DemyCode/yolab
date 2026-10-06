import { useMemo, useState, type ReactNode } from "react";
import { Package, Search, X } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Input, Select } from "@/components/ui/input";
import { AppCard, FeaturedAppCard } from "@/components/AppCard";
import { Skeleton, EmptyState } from "@/components/ui/feedback";
import { useApi } from "@/lib/useResource";
import { installedByChart } from "@/lib/apps";
import {
  GROUPS,
  countByGroup,
  groupFor,
  groupLabel,
  taglineFor,
} from "@/catalog/meta";
import { AppSources } from "@/components/AppSources";
import { AddFromBackupButton } from "@/components/AddFromBackup";
import { cn } from "@/lib/utils";
import { AnimatedList, RollingNumber } from "@/components/motion";
import {
  COLLECTIONS,
  inCollection,
  sortApps,
  statsById,
  type AppStats,
  type SortOrder,
} from "@/lib/store";
import type { AppInfo, CatalogApp } from "@/types/apps";

type Installed = "any" | "installed" | "not-installed";

const FEATURED = "start-here";

function catalogKey(app: CatalogApp): string {
  return `${app.repo}/${app.id}`;
}

function sourceLabel(s: string): string {
  if (s === "official") return "YoLab catalog";
  if (s === "custom") return "Your own";
  return s;
}

function Chip({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      aria-pressed={active}
      className={cn(
        "inline-flex shrink-0 items-center gap-1.5 rounded-full border px-3.5 py-1.5 text-sm transition-colors",
        active
          ? "border-fg bg-fg text-bg"
          : "border-border-strong bg-surface text-fg-muted hover:text-fg",
      )}
    >
      {children}
    </button>
  );
}

function ChipCount({ n, active }: { n: number; active: boolean }) {
  return (
    <span
      className={cn(
        "font-mono text-xs tabular-nums",
        active ? "text-bg/70" : "text-fg-subtle",
      )}
    >
      {n}
    </span>
  );
}

export function AppsPage() {
  const [query, setQuery] = useState("");
  const [activeGroup, setActiveGroup] = useState<string | null>(null);
  const [source, setSource] = useState("any");
  const [installed, setInstalled] = useState<Installed>("any");
  const [order, setOrder] = useState<SortOrder>("popular");

  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const apps = useApi<AppInfo[]>("apps", "/api/apps");
  const stats = useApi<AppStats[]>("store-stats", "/api/store/stats");
  const statsMap = useMemo(() => statsById(stats.data), [stats.data]);

  const installedCounts = useMemo(
    () => installedByChart(apps.data),
    [apps.data],
  );

  const all = useMemo(() => catalog.data ?? [], [catalog.data]);

  const sources = useMemo(
    () => [...new Set(all.map((a) => a.repo))].sort(),
    [all],
  );

  const q = query.trim().toLowerCase();
  const searching = q !== "";
  const filtersOn = source !== "any" || installed !== "any";
  const narrowed = searching || filtersOn;

  const kept = useMemo(
    () =>
      all.filter((a) => {
        if (source !== "any" && a.repo !== source) return false;
        const n = installedCounts.get(a.id) ?? 0;
        if (installed === "installed" && n === 0) return false;
        if (installed === "not-installed" && n > 0) return false;
        if (!q) return true;
        return `${a.name} ${a.id} ${taglineFor(a)} ${a.description}`
          .toLowerCase()
          .includes(q);
      }),
    [all, source, installed, installedCounts, q],
  );

  const groupCounts = useMemo(() => countByGroup(kept), [kept]);

  const matches = useMemo(() => {
    const found = kept.filter(
      (a) => !activeGroup || groupFor(a) === activeGroup,
    );
    if (!q) return sortApps(found, order, statsMap);
    const rank = (a: CatalogApp) =>
      a.name.toLowerCase().startsWith(q)
        ? 0
        : a.name.toLowerCase().includes(q)
          ? 1
          : 2;
    return found.sort(
      (a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name),
    );
  }, [kept, activeGroup, q, order, statsMap]);

  const grouped = useMemo(() => {
    if (narrowed || activeGroup) return [];
    const byGroup = new Map<string, CatalogApp[]>();
    for (const app of matches) {
      const g = groupFor(app);
      byGroup.set(g, [...(byGroup.get(g) ?? []), app]);
    }
    const ids = GROUPS.map((g) => g.id);
    return [...byGroup.entries()].sort(
      (a, b) => ids.indexOf(a[0]) - ids.indexOf(b[0]),
    );
  }, [matches, narrowed, activeGroup]);

  const featured = useMemo(
    () => inCollection(all, FEATURED, statsMap),
    [all, statsMap],
  );

  function clearAll() {
    setQuery("");
    setSource("any");
    setInstalled("any");
    setActiveGroup(null);
  }

  const card = (app: CatalogApp) => (
    <AppCard
      app={app}
      count={installedCounts.get(app.id) ?? 0}
      stats={statsMap.get(app.id)}
    />
  );

  const grid = (list: CatalogApp[]) => (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
      <AnimatedList items={list} keyOf={catalogKey}>
        {card}
      </AnimatedList>
    </div>
  );

  return (
    <Page
      wide
      title="Apps"
      subtitle="Everything here runs at home, on your own machines."
      action={<AddFromBackupButton />}
    >
      <div className="relative">
        <Search className="pointer-events-none absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-fg-subtle" />
        <Input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={
            all.length > 0
              ? `Search ${all.length} apps, like photos, films or passwords`
              : "Search apps"
          }
          className="h-12 pl-10 text-base"
          type="search"
          aria-label="Search apps"
        />
      </div>

      {catalog.loading ? (
        <div className="mt-8 space-y-8">
          <Skeleton className="h-72 rounded-card" />
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {Array.from({ length: 6 }, (_, i) => (
              <Skeleton key={i} className="h-[6rem]" />
            ))}
          </div>
        </div>
      ) : all.length === 0 ? (
        <div className="mt-8">
          <EmptyState
            icon={<Package className="h-6 w-6" />}
            title="No apps yet"
            body="Nothing is available to install yet. Add a source below, or sync the catalog."
          />
        </div>
      ) : (
        <>
          {!narrowed && featured.length > 0 && (
            <section className="mt-8 rounded-card bg-surface-2 p-4 sm:p-6">
              <h2 className="font-display text-xl font-semibold tracking-tight text-fg md:text-2xl">
                {COLLECTIONS.find((c) => c.id === FEATURED)?.title}
              </h2>
              <p className="mt-1 text-sm text-fg-muted">
                {COLLECTIONS.find((c) => c.id === FEATURED)?.blurb}. Pick one to
                see what it needs, then install it in a minute.
              </p>
              <div className="mt-5 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                <AnimatedList items={featured} keyOf={catalogKey}>
                  {(app) => (
                    <FeaturedAppCard
                      app={app}
                      count={installedCounts.get(app.id) ?? 0}
                      stats={statsMap.get(app.id)}
                    />
                  )}
                </AnimatedList>
              </div>
            </section>
          )}

          {!narrowed &&
            COLLECTIONS.filter((c) => c.id !== FEATURED).map((c) => {
              const row = inCollection(all, c.id, statsMap);
              if (row.length === 0) return null;
              return (
                <section key={c.id} className="mt-10">
                  <h2 className="text-base font-semibold text-fg">{c.title}</h2>
                  <p className="mt-0.5 text-sm text-fg-muted">{c.blurb}</p>
                  <div className="-mx-5 mt-3 flex snap-x snap-mandatory gap-3 overflow-x-auto px-5 pb-2 [scrollbar-width:thin] md:mx-0 md:px-0">
                    <AnimatedList
                      items={row}
                      keyOf={catalogKey}
                      itemClassName="w-[17.5rem] shrink-0 snap-start"
                    >
                      {card}
                    </AnimatedList>
                  </div>
                </section>
              );
            })}

          <section className={narrowed ? "mt-6" : "mt-12"}>
            <div className="flex flex-wrap items-end justify-between gap-x-6 gap-y-3">
              <div className="flex items-baseline gap-3">
                <h2 className="font-display text-xl font-semibold tracking-tight text-fg md:text-2xl">
                  {searching ? `Results for “${query.trim()}”` : "All apps"}
                </h2>
                <span className="font-mono text-sm tabular-nums text-fg-subtle">
                  <RollingNumber value={matches.length} />
                </span>
                {(narrowed || activeGroup) && (
                  <button
                    onClick={clearAll}
                    className="inline-flex items-center gap-1 text-sm text-fg-muted hover:text-fg"
                  >
                    <X className="h-3.5 w-3.5" aria-hidden />
                    Clear
                  </button>
                )}
              </div>
              <div className="flex flex-wrap items-center gap-2 text-sm">
                <Select
                  value={order}
                  onChange={(e) => setOrder(e.target.value as SortOrder)}
                  className="h-9 w-auto"
                  aria-label="Sort"
                  disabled={searching}
                >
                  <option value="popular">Most popular</option>
                  <option value="updated">Recently updated</option>
                  <option value="name">A to Z</option>
                </Select>
                <Select
                  value={installed}
                  onChange={(e) => setInstalled(e.target.value as Installed)}
                  className="h-9 w-auto"
                  aria-label="Installed or not"
                >
                  <option value="any">Installed or not</option>
                  <option value="not-installed">Not installed yet</option>
                  <option value="installed">Installed</option>
                </Select>
                {sources.length > 1 && (
                  <Select
                    value={source}
                    onChange={(e) => setSource(e.target.value)}
                    className="h-9 w-auto"
                    aria-label="Source"
                  >
                    <option value="any">Every source</option>
                    {sources.map((s) => (
                      <option key={s} value={s}>
                        {sourceLabel(s)}
                      </option>
                    ))}
                  </Select>
                )}
              </div>
            </div>

            <div className="-mx-5 mt-4 flex gap-2 overflow-x-auto px-5 pb-1 [scrollbar-width:thin] md:mx-0 md:flex-wrap md:px-0">
              <Chip
                active={activeGroup === null}
                onClick={() => setActiveGroup(null)}
              >
                Everything
                <ChipCount n={kept.length} active={activeGroup === null} />
              </Chip>
              {GROUPS.filter((g) => groupCounts.has(g.id)).map((g) => (
                <Chip
                  key={g.id}
                  active={activeGroup === g.id}
                  onClick={() =>
                    setActiveGroup(g.id === activeGroup ? null : g.id)
                  }
                >
                  {g.label}
                  <ChipCount
                    n={groupCounts.get(g.id) ?? 0}
                    active={activeGroup === g.id}
                  />
                </Chip>
              ))}
            </div>

            <div className="mt-5">
              {matches.length === 0 ? (
                <EmptyState
                  icon={<Package className="h-6 w-6" />}
                  title="Nothing matches that"
                  body="No app matches the search and filters. Try another word, or clear them."
                />
              ) : grouped.length > 0 ? (
                <div className="space-y-8">
                  {grouped.map(([groupId, groupApps]) => (
                    <div key={groupId}>
                      <h3 className="mb-3 text-sm font-semibold text-fg-muted">
                        {groupLabel(groupId)}
                      </h3>
                      {grid(groupApps)}
                    </div>
                  ))}
                </div>
              ) : (
                grid(matches)
              )}
            </div>
          </section>
        </>
      )}

      <AppSources onChanged={() => void catalog.refresh()} />
    </Page>
  );
}
