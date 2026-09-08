import { useMemo, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { AlertTriangle, Plus, RotateCcw, Sparkles, Trash2 } from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppIconTile } from "@/components/AppIcon";
import { AppTile, AppTileSkeleton } from "@/components/AppTile";
import { Banner, EmptyState, ServiceTrouble } from "@/components/ui/feedback";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { api } from "@/lib/api";
import { useResource } from "@/lib/useResource";
import { appDisplayName, catalogEntry } from "@/lib/apps";
import type { AppInfo, CatalogApp } from "@/types/apps";
import type { ClusterHealth } from "@/types/health";

/** One app with permanently lost data, and whether it can come back. */
interface DamagedApp {
  namespace: string;
  instance_name: string;
  app_id: string;
  restorable: boolean;
  backup_age_hours: number | null;
}

/** `GET /api/backups/damage` — only non-empty while storage is unrecoverable. */
interface DamageResponse {
  unrecoverable: boolean;
  lost_disks: number;
  restorable_count: number;
  delete_count: number;
  apps: DamagedApp[];
}

/**
 * Chooses the single most important thing to say, or says nothing.
 *
 * The old shell showed a permanent "Storage healthy" chip in the sidebar plus a
 * banner for every issue at once. Both are wrong for this audience: a green
 * tick teaches people to monitor Ceph, and a stack of warnings they cannot act
 * on teaches them to ignore the whole strip. Silence is the success case.
 */
interface Concern {
  tone: "info" | "warning" | "error";
  title: string;
  body: string;
  /** How many further issues were folded away behind this one. */
  more?: number;
}

function topConcern(health: ClusterHealth | undefined): Concern | null {
  if (!health) return null;

  // Expected, temporary states. These are not problems and must not be dressed
  // up as ones — a box that just booted is not a box in trouble.
  //
  // They are checked AFTER severity, and that ordering is the whole point. Both are
  // guesses about WHY something looks off, and both guess wrong in exactly the
  // situation where being wrong costs the most. Observed live: a disk was pulled from
  // a cluster keeping one copy, 63 of 81 placement groups went unreadable, and this
  // page said "Preparing a new disk — you can keep using everything while this
  // finishes", because `provisioning` was checked first and its backing signal
  // (`in > up`) is also precisely what a dead disk looks like.
  //
  // A reassuring explanation may only ever apply when nothing is actually wrong.
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

function formatBackupAge(hours: number | null): string {
  if (hours === null) return "Never backed up";
  if (hours < 1) return "Backed up less than an hour ago";
  if (hours < 24) {
    const h = Math.round(hours);
    return `Backed up ${h} hour${h === 1 ? "" : "s"} ago`;
  }
  const d = Math.round(hours / 24);
  return `Backed up ${d} day${d === 1 ? "" : "s"} ago`;
}

/**
 * The "apps with lost data" section — rendered only when storage has suffered
 * *unrecoverable* loss, below the working apps.
 *
 * Every affected app is already classified by the backend into one of exactly
 * two fates, so this is a triage the owner reads and confirms rather than a
 * puzzle to solve: restorable → Restore, or no backup → Delete. A "down but
 * safe" app never appears here (it is not on the backend's list at all); it
 * stays in the grid above, restarting on its own.
 */
function LostAppsSection({
  damage,
  installed,
  catalogApps,
  onChanged,
}: {
  damage: DamageResponse;
  installed: AppInfo[];
  catalogApps: CatalogApp[];
  onChanged: () => void;
}) {
  const navigate = useNavigate();
  const [busy, setBusy] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const restorable = damage.apps.filter((a) => a.restorable);

  async function restore(namespaces: string[]) {
    setError(null);
    try {
      await api.post("/api/backups/dr/start", {
        namespaces,
        rebuild_storage: true,
      });
      // The restore takes over the Backups page; go watch it there.
      navigate("/box/backups");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not start the restore");
    }
  }

  async function remove(instanceName: string) {
    setBusy(instanceName);
    setError(null);
    try {
      await api.del(`/api/apps/${instanceName}`);
      setConfirm(null);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the app");
    } finally {
      setBusy(null);
    }
  }

  return (
    <section className="rounded-card border border-danger/30 bg-danger-soft/40 p-4">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h2 className="flex items-center gap-2 text-sm font-semibold text-danger">
            <AlertTriangle className="h-4 w-4" />
            Apps with lost data
          </h2>
          <p className="mt-0.5 text-xs text-fg-muted">
            {damage.lost_disks > 0
              ? `${damage.lost_disks} disk${damage.lost_disks === 1 ? "" : "s"} gone — `
              : ""}
            {damage.apps.length} app{damage.apps.length === 1 ? "" : "s"} lost
            their data · {damage.restorable_count} can be restored ·{" "}
            {damage.delete_count} can not
          </p>
        </div>
        {restorable.length > 1 && (
          <Button
            size="sm"
            variant="primary"
            loading={busy === "__all__"}
            onClick={() => {
              if (confirm === "__all__") {
                void restore(restorable.map((a) => a.namespace));
                setBusy("__all__");
              } else {
                setConfirm("__all__");
              }
            }}
          >
            {confirm === "__all__"
              ? "Restore all now?"
              : `Restore all ${restorable.length}`}
          </Button>
        )}
      </div>

      {error && <p className="mt-2 text-xs text-danger">{error}</p>}

      <ul className="mt-3 space-y-2">
        {damage.apps.map((d) => {
          const app = installed.find(
            (a) => a.instance_name === d.instance_name,
          );
          const entry = app ? catalogEntry(app, catalogApps) : undefined;
          const name = app ? appDisplayName(app, catalogApps) : d.instance_name;
          const icon = entry?.icon ?? "📦";

          const deleting = confirm === d.instance_name;
          return (
            <li
              key={d.namespace}
              className="flex items-center gap-3 rounded-card border border-border bg-surface/60 px-3 py-2"
            >
              <AppIconTile appId={d.app_id} icon={icon} name={name} size="sm" />
              <div className="min-w-0 flex-1">
                <p className="flex items-center gap-1.5 truncate text-sm font-medium text-fg">
                  {name}
                  {d.restorable ? (
                    <span className="shrink-0 rounded bg-danger-soft px-1.5 py-0.5 text-[10px] font-medium text-danger">
                      Data lost
                    </span>
                  ) : (
                    <span className="shrink-0 rounded bg-warning-soft px-1.5 py-0.5 text-[10px] font-medium text-warning">
                      No backup
                    </span>
                  )}
                </p>
                <p className="truncate text-xs text-fg-muted">
                  {d.restorable
                    ? formatBackupAge(d.backup_age_hours)
                    : "Its files were never backed up — nothing to bring back"}
                </p>
              </div>

              {d.restorable ? (
                <Button
                  size="sm"
                  variant="outline"
                  loading={busy === d.namespace}
                  onClick={() => {
                    setBusy(d.namespace);
                    void restore([d.namespace]);
                  }}
                  className="shrink-0 text-primary"
                >
                  <RotateCcw className="h-3.5 w-3.5" />
                  Restore
                </Button>
              ) : (
                <Button
                  size="sm"
                  variant="quiet"
                  loading={busy === d.instance_name}
                  onClick={() => {
                    if (deleting) {
                      void remove(d.instance_name);
                    } else {
                      setConfirm(d.instance_name);
                    }
                  }}
                  className="shrink-0"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                  {deleting ? "Delete permanently?" : "Delete"}
                </Button>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}

export function HomePage() {
  const apps = useResource<AppInfo[]>("apps", () => api.get("/api/apps"), {
    pollMs: 10_000,
  });
  const catalog = useResource<CatalogApp[]>("catalog", () =>
    api.get("/api/apps/catalog"),
  );
  const health = useResource<ClusterHealth>(
    "health",
    () => api.get("/api/cluster/health"),
    { pollMs: 20_000 },
  );

  const concern = topConcern(health.data);
  const catalogApps = useMemo(() => catalog.data ?? [], [catalog.data]);
  const installed = useMemo(() => apps.data ?? [], [apps.data]);

  // Only asked for while storage is unrecoverable — otherwise the endpoint is a
  // no-op and fetching it on every visit is noise. `key=null` disables the fetch.
  const damage = useResource<DamageResponse>(
    health.data?.storage_unrecoverable ? "backups-damage" : null,
    () => api.get("/api/backups/damage"),
    { pollMs: 15_000 },
  );

  // Lost apps come out of the working grid and move into the section below, so a
  // casualty is never shown twice, once as healthy and once as lost.
  const lostNamespaces = useMemo(
    () => new Set((damage.data?.apps ?? []).map((a) => a.instance_name)),
    [damage.data],
  );
  const workingApps = useMemo(
    () => installed.filter((a) => !lostNamespaces.has(a.instance_name)),
    [installed, lostNamespaces],
  );

  return (
    <Page wide>
      <header className="mb-6">
        <h1 className="font-display text-[1.75rem] leading-tight text-fg md:text-4xl">
          Your services
        </h1>
        <p className="mt-1 text-sm text-fg-muted">
          Everything running at home. Tap one to open it.
        </p>
      </header>

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
          {/* Everything else is folded behind one link rather than stacked as
              more banners — see the note on `topConcern`. */}
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
        // Not "nothing installed" — the list never loaded at all. Telling
        // someone their apps are gone because of a moment's outage is worse
        // than telling them nothing.
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
            {workingApps.map((app) => {
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

          {damage.data && damage.data.apps.length > 0 && (
            <LostAppsSection
              damage={damage.data}
              installed={installed}
              catalogApps={catalogApps}
              onChanged={() => {
                void apps.refresh();
                void damage.refresh();
              }}
            />
          )}
        </>
      )}
    </Page>
  );
}
