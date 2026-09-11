import type { CacheMeta } from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * Says whether the numbers next to it were just measured or merely remembered.
 *
 * THIS IS A SAFETY CONTROL, NOT DECORATION.
 *
 * `useResource` used to render last-known values from localStorage and that was
 * deliberately deleted, because during an incident a confidently-rendered stale
 * number — "1.2 TB free" from before a disk failed, a disk still shown "in use"
 * after it was pulled — is worse than a spinner. Caching is back, server-side
 * and much shorter-lived, and it is only acceptable because of this badge: the
 * page shows a remembered value instantly, and says so, until the real one
 * arrives a second or five later.
 *
 * So it renders NOTHING once the value is live. An indicator that is always on
 * screen is one nobody reads; this appears exactly when the distinction matters
 * and disappears the moment it stops mattering, which is within seconds.
 *
 * The age is shown rather than a bare "cached" because age is the part an
 * operator can act on. Eight seconds old is fine while watching a rebuild;
 * fifty seconds old, during a disk failure, is not.
 *
 * It deliberately does NOT tick upward while the refresh runs. Doing that needs
 * a clock read during render, which is impure and forbidden by the lint rules
 * here for good reason — and it buys nothing, because the badge's whole lifetime
 * is the few seconds between the two frames of one request.
 */
export function CacheBadge({
  cache,
  className,
}: {
  cache: CacheMeta | null;
  className?: string;
}) {
  if (cache === null) return null;
  if (cache.state !== "hit" && cache.state !== "stale") return null;

  const seconds = Math.max(1, Math.round(cache.ageMs / 1000));
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 text-xs text-fg-muted",
        className,
      )}
      title={`Showing a value from ${seconds}s ago while the box recalculates it.`}
    >
      <span
        aria-hidden
        className="size-1.5 rounded-full bg-amber-500 animate-pulse"
      />
      {seconds}s ago · refreshing
    </span>
  );
}
