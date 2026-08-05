import { MoreHorizontal } from "lucide-react";
import { StatusDot } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/feedback";
import { cn } from "@/lib/utils";
import { appState, appStateLabel } from "@/lib/apps";
import type { AppInfo } from "@/types/apps";

/**
 * One installed app.
 *
 * Tapping it opens the app. Not a detail page — the app.
 *
 * This is the single biggest behavioural change from the old UI, where every
 * app in the list led to a page of pods, install logs and Helm values. That is
 * an operator's view offered as the default, and it stands between the owner
 * and the thing they actually came to use. Everything operational still exists,
 * behind the "…" button, which is where it belongs: reachable, not in the way.
 */
export function AppTile({
  app,
  name,
  icon,
  url,
  onDetails,
}: {
  app: AppInfo;
  name: string;
  icon: string;
  url: string | null;
  onDetails: () => void;
}) {
  const state = appState(app);
  const label = appStateLabel(state);
  const disabled = state === "removing" || !url;

  const tone =
    state === "removing" ? "warn" : state === "starting" ? "busy" : "ok";

  const body = (
    <>
      <div className="relative mb-3">
        <div
          className={cn(
            "flex h-16 w-16 items-center justify-center rounded-tile bg-surface-2 text-3xl transition-transform",
            !disabled && "group-hover:scale-105",
          )}
          aria-hidden
        >
          {icon || "📦"}
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
    </>
  );

  return (
    <div className="group relative">
      {disabled ? (
        <div className="flex flex-col items-center rounded-card p-3 opacity-60">
          {body}
        </div>
      ) : (
        <a
          href={url}
          target="_blank"
          rel="noopener noreferrer"
          className="flex flex-col items-center rounded-card p-3 transition-colors hover:bg-surface active:scale-[0.97]"
        >
          {body}
        </a>
      )}

      {/* Always visible on touch, where there is no hover to reveal it. */}
      <button
        onClick={onDetails}
        aria-label={`${name} settings`}
        className="absolute right-1 top-1 rounded-lg p-1.5 text-fg-subtle opacity-100 transition hover:bg-surface-2 hover:text-fg md:opacity-0 md:group-hover:opacity-100 md:focus-visible:opacity-100"
      >
        <MoreHorizontal className="h-4 w-4" />
      </button>
    </div>
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
