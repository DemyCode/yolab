import { useMemo, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { Search, SlidersHorizontal } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Input } from "@/components/ui/input";
import { AppCard } from "@/components/AppCard";
import { Skeleton, EmptyState } from "@/components/ui/feedback";
import { useApi } from "@/lib/useResource";
import { installedByChart } from "@/lib/apps";
import { GROUPS, groupFor, groupLabel } from "@/catalog/meta";
import { AppSources } from "@/components/AppSources";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp } from "@/types/apps";


export function DiscoverPage() {
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [activeGroup, setActiveGroup] = useState<string | null>(null);

  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const apps = useApi<AppInfo[]>("apps", "/api/apps");

  const installedCounts = useMemo(
    () => installedByChart(apps.data),
    [apps.data],
  );

  const grouped = useMemo(() => {
    const matches = catalog.data ?? [];
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
  }, [catalog.data, activeGroup]);

  return (
    <Page
      wide
      title="Add a service"
      subtitle="Everything here runs at home, on your own machines."
    >
      {
}
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

      {
}
      <AppSources onChanged={() => void catalog.refresh()} />
    </Page>
  );
}
