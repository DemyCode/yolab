import { Link } from "react-router-dom";
import { AppIcon } from "@/components/AppIcon";
import { StatusDot } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/feedback";
import { cn } from "@/lib/utils";
import { appState, appStateLabel } from "@/lib/apps";
import type { AppInfo } from "@/types/apps";

/**
 * One installed app, leading to that app's page.
 *
 * An earlier version made the tile a direct link to the app itself, on the
 * theory that opening it is what people came to do. That is wrong for anything
 * that is not a website: minecraft and valheim publish a server address to
 * paste into a game client, not a URL, and several charts publish more than
 * one link — so "the" address does not always exist and, when it does, is not
 * always the only one worth having. The tile leads somewhere that can show all
 * of it.
 */
export function AppTile({
  app,
  name,
  icon,
}: {
  app: AppInfo;
  name: string;
  icon: string;
}) {
  const state = appState(app);
  const label = appStateLabel(state);
  const tone =
    state === "removing" ? "warn" : state === "starting" ? "busy" : "ok";

  return (
    <Link
      to={`/app/${app.instance_name}`}
      className={cn(
        "group flex flex-col items-center rounded-card p-3 transition-colors hover:bg-surface active:scale-[0.97]",
        state === "removing" && "opacity-60",
      )}
    >
      <div className="relative mb-3">
        <div className="flex h-16 w-16 items-center justify-center rounded-tile bg-surface-2 transition-transform group-hover:scale-105">
          <AppIcon icon={icon} name={name} className="h-9 w-9 text-3xl" />
        </div>
        <StatusDot
          tone={tone}
          pulse={state === "starting"}
          className="absolute -right-0.5 -top-0.5"
        />
      </div>
      <span className="line-clamp-1 text-sm font-medium text-fg">{name}</span>
      {label && (
        <span className="mt-0.5 line-clamp-1 text-xs text-fg-muted">
          {label}
        </span>
      )}
    </Link>
  );
}

export function AppTileSkeleton() {
  return (
    <div className="flex flex-col items-center p-3">
      <Skeleton className="mb-3 h-16 w-16 rounded-tile" />
      <Skeleton className="h-3.5 w-16" />
    </div>
  );
}
