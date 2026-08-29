import { useMemo, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { Search, SlidersHorizontal } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Input } from "@/components/ui/input";
import { AppCard } from "@/components/AppCard";
import { Skeleton, EmptyState } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import { GROUPS, groupFor, groupLabel } from "@/catalog/meta";
import { AppSources } from "@/components/AppSources";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp } from "@/types/apps";

/**
 * Already having an app is not a reason to be refused another.
 *
 * The first version greyed out anything installed, which quietly forbade a
 * perfectly ordinary thing: a family photo library and a private one, a work
 * password vault and a personal one, a test blog beside the real one. The
 * backend never had that limitation — every install gets its own namespace —
 * so the card stays clickable and just says how many you already have.
 */

/**
 * The catalog, as a shop rather than a chart index.
 *
 * Two things do the work here. Apps are described by what they replace rather
 * than by what they are ("Your photos, like Google Photos" instead of
 * "Self-hosted photo and video backup"), and they are grouped by what someone
 * came looking for rather than by the chart's `category` annotation, which was
 * written for us and not for them.
 */
export function DiscoverPage() {
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [activeGroup, setActiveGroup] = useState<string | null>(null);

  const catalog = useResource<CatalogApp[]>("catalog", () =>
    api.get("/api/apps/catalog"),
  );
  const apps = useResource<AppInfo[]>("apps", () => api.get("/api/apps"));

  /** chart id → how many copies are installed. */
  const installedCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const a of apps.data ?? []) {
      counts.set(a.app_id, (counts.get(a.app_id) ?? 0) + 1);
    }
    return counts;
  }, [apps.data]);

  // Browsing shows everything; narrowing is the search page's job.
  const matches = catalog.data ?? [];

  const grouped = useMemo(() => {
    const byGroup = new Map<string, CatalogApp[]>();
    for (const app of matches) {
      const g = groupFor(app);
      if (activeGroup && g !== activeGroup) continue;
      const list = byGroup.get(g) ?? [];
      list.push(app);
      byGroup.set(g, list);
    }
    const order = GROUPS.map((g) => g.id);
    return [...byGroup.entries()].sort(
      (a, b) => order.indexOf(a[0]) - order.indexOf(b[0]),
    );
  }, [matches, activeGroup]);

  return (
    <Page
      wide
      title="Add a service"
      subtitle="Everything here runs at home, on your own machines."
    >
      {/* Typing here hands off to the search page rather than filtering in place.
          Browsing and narrowing want different layouts — one wants grouping and
          room, the other wants a flat ranked list — and this page trying to be
          both is what turned it into 70 unknown names under a search box. */}
      <div className="mb-4 flex gap-2">
        <div className="relative flex-1">
          <Search className="pointer-events-none absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-fg-subtle" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && query.trim()) {
                navigate(`/search?q=${encodeURIComponent(query.trim())}`);
              }
            }}
            placeholder="Search — try 'photos' or 'netflix'"
            className="pl-10"
            type="search"
            aria-label="Search apps"
          />
        </div>
        <Link
          to={
            query.trim()
              ? `/search?q=${encodeURIComponent(query.trim())}`
              : "/search"
          }
          className="inline-flex shrink-0 items-center gap-1.5 rounded-md border border-border px-3 text-sm text-fg-muted transition-colors hover:border-border-strong hover:text-fg"
        >
          <SlidersHorizontal className="h-3.5 w-3.5" />
          Filters
        </Link>
      </div>

      <div className="-mx-5 mb-6 flex gap-2 overflow-x-auto px-5 pb-1 md:mx-0 md:px-0">
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

      {catalog.loading ? (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {Array.from({ length: 9 }, (_, i) => (
            <Skeleton key={i} className="h-[5.5rem]" />
          ))}
        </div>
      ) : grouped.length === 0 ? (
        <EmptyState
          icon={<Search className="h-6 w-6" />}
          title="Nothing matches that"
          body={`No app matches "${query}". Try a different word, or browse a category above.`}
        />
      ) : (
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
      )}

      {/* Refreshing the catalog after a source changes, so a newly added one's
          apps appear in the grid above without a reload. */}
      <AppSources onChanged={() => void catalog.refresh()} />
    </Page>
  );
}
