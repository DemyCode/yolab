import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { Check, Search } from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppIcon } from "@/components/AppIcon";
import { Input } from "@/components/ui/input";
import { Skeleton, EmptyState } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import { GROUPS, groupFor, groupLabel, taglineFor } from "@/catalog/meta";
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
function AppCard({ app, count }: { app: CatalogApp; count: number }) {
  return (
    <Link
      to={`/add/${app.id}`}
      className="group flex items-start gap-3.5 rounded-card border border-border bg-surface p-4 transition hover:border-border-strong hover:shadow-[var(--shadow-card)] active:scale-[0.99]"
    >
      <div className="flex h-12 w-12 shrink-0 items-center justify-center rounded-xl bg-surface-2">
        <AppIcon icon={app.icon} name={app.name} className="h-7 w-7 text-2xl" />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium text-fg">{app.name}</span>
          {count > 0 && (
            <span className="flex shrink-0 items-center gap-1 text-xs text-success">
              <Check className="h-3.5 w-3.5" />
              {count === 1 ? "Installed" : `${count} installed`}
            </span>
          )}
        </div>
        <p className="mt-0.5 line-clamp-2 text-sm text-fg-muted">
          {taglineFor(app)}
        </p>
        {count > 0 && (
          <p className="mt-1 text-xs text-fg-subtle opacity-0 transition-opacity group-hover:opacity-100">
            Add another copy
          </p>
        )}
      </div>
    </Link>
  );
}

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

  const matches = useMemo(() => {
    const all = catalog.data ?? [];
    const q = query.trim().toLowerCase();
    if (!q) return all;
    // Search the tagline too, so "netflix" finds Jellyfin and "1password"
    // finds Vaultwarden — people search for the thing they already know.
    return all.filter((a) =>
      `${a.name} ${a.id} ${taglineFor(a)} ${a.description}`
        .toLowerCase()
        .includes(q),
    );
  }, [catalog.data, query]);

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
    <Page wide title="Add an app" subtitle="Everything here runs on your box.">
      <div className="relative mb-4">
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
    </Page>
  );
}
