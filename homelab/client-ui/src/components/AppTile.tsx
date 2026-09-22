import { Link } from "react-router-dom";
import { AppIconTile } from "@/components/AppIcon";
import { StatusDot } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/feedback";
import { cn } from "@/lib/utils";
import { appLabel, appState } from "@/lib/apps";
import type { AppInfo } from "@/types/apps";

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
  const label = appLabel(app, state);
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
        <AppIconTile
          appId={app.app_id}
          icon={icon}
          name={name}
          className="shadow-[var(--shadow-card)] transition-all duration-200 group-hover:-translate-y-0.5 group-hover:shadow-[var(--shadow-lift)]"
        />
        <StatusDot
          tone={tone}
          pulse={state === "starting"}
          className="absolute -right-0.5 -top-0.5"
        />
      </div>
      <span className="line-clamp-1 text-sm font-medium text-fg">{name}</span>
      {}
      {label && (
        <span className="mt-0.5 line-clamp-2 text-balance text-center text-xs text-fg-muted">
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
