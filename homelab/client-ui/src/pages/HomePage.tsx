import { useMemo } from "react";
import { Link } from "react-router-dom";
import { Plus, Sparkles } from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppTile, AppTileSkeleton } from "@/components/AppTile";
import { Banner, EmptyState, ServiceTrouble } from "@/components/ui/feedback";
import { buttonClass } from "@/components/ui/button-variants";
import { useApi } from "@/lib/useResource";
import { CacheDot } from "@/components/CacheDot";
import { HealBanner } from "@/components/ForceHeal";
import { appDisplayName, catalogEntry } from "@/lib/apps";
import type { AppInfo, CatalogApp } from "@/types/apps";
import type { ClusterHealth } from "@/types/health";

interface Concern {
  tone: "info" | "warning" | "error";
  title: string;
  body: string;
  more?: number;
}

function topConcern(health: ClusterHealth | undefined): Concern | null {
  if (!health) return null;

  if (health.level !== "error") {
    if (health.starting) {
      return {
        tone: "info" as const,
        title: "Your storage is starting up",
        body: "This usually takes a minute after the machine boots. Apps will come back on their own.",
      };
    }
    if (health.provisioning) {
      return {
        tone: "info" as const,
        title: "Preparing a new disk",
        body: "You can keep using everything while this finishes.",
      };
    }
  }
  if (health.level === "ok") return null;

  const worst =
    health.issues.find((i) => i.level === "error") ?? health.issues[0];
  return {
    tone: health.level === "error" ? ("error" as const) : ("warning" as const),
    title: worst?.title ?? health.title,
    body: worst?.description ?? health.message,
    more: Math.max(0, health.issues.length - 1),
  };
}

export function HomePage() {
  const apps = useApi<AppInfo[]>("apps", "/api/apps", {
    pollMs: 10_000,
  });
  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const health = useApi<ClusterHealth>("health", "/api/cluster/health", {
    pollMs: 20_000,
  });

  const concern = topConcern(health.data);
  const catalogApps = useMemo(() => catalog.data ?? [], [catalog.data]);
  const installed = useMemo(() => apps.data ?? [], [apps.data]);

  return (
    <Page wide>
      <header className="mb-6 flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="font-display text-[1.75rem] leading-tight text-fg md:text-4xl">
            Your services
          </h1>
          <p className="mt-1 flex items-center gap-2 text-sm text-fg-muted">
            Everything running at home. Tap one to open it.
            {}
            <CacheDot cache={health.cache ?? apps.cache} />
          </p>
        </div>
      </header>

      {}
      <HealBanner className="mb-6" />

      {concern && (
        <Banner
          tone={concern.tone}
          title={concern.title}
          className="mb-6"
          action={
            concern.tone !== "info" ? (
              <Link
                to="/box/storage"
                className={buttonClass({ size: "sm", variant: "secondary" })}
              >
                Look at storage
              </Link>
            ) : undefined
          }
        >
          {concern.body}
          {}
          {(concern.more ?? 0) > 0 && (
            <>
              {" "}
              <Link to="/box/storage" className="underline underline-offset-2">
                {concern.more} other {concern.more === 1 ? "issue" : "issues"}
              </Link>
            </>
          )}
        </Banner>
      )}

      {apps.loading ? (
        <div className="grid grid-cols-3 gap-2 sm:grid-cols-4 md:grid-cols-6">
          {Array.from({ length: 8 }, (_, i) => (
            <AppTileSkeleton key={i} />
          ))}
        </div>
      ) : apps.error && !apps.data ? (
        <ServiceTrouble onRetry={apps.refresh} />
      ) : installed.length === 0 ? (
        <EmptyState
          icon={<Sparkles className="h-6 w-6" />}
          title="Nothing installed yet"
          body="Your home server is ready. Pick something you'd like to stop paying a subscription for."
          action={
            <Link to="/add" className={buttonClass()}>
              <Plus className="h-4 w-4" />
              Browse apps
            </Link>
          }
        />
      ) : (
        <>
          <div className="grid grid-cols-3 gap-2 sm:grid-cols-4 md:grid-cols-6">
            {installed.map((app) => {
              const entry = catalogEntry(app, catalogApps);
              return (
                <AppTile
                  key={app.instance_name}
                  app={app}
                  name={appDisplayName(app, catalogApps)}
                  icon={entry?.icon ?? "📦"}
                />
              );
            })}

            <Link
              to="/add"
              className="flex flex-col items-center rounded-card p-3 transition-colors hover:bg-surface active:scale-[0.97]"
            >
              <div className="mb-3 flex h-16 w-16 items-center justify-center rounded-tile border-2 border-dashed border-border-strong text-fg-subtle">
                <Plus className="h-6 w-6" />
              </div>
              <span className="text-sm font-medium text-fg-muted">Add</span>
            </Link>
          </div>
        </>
      )}
    </Page>
  );
}
