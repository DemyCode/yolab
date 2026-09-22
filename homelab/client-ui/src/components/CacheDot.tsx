import type { CacheMeta } from "@/lib/api";
import { cn } from "@/lib/utils";

export function CacheDot({
  cache,
  className,
}: {
  cache: CacheMeta | null;
  className?: string;
}) {
  if (cache === null) return null;
  if (cache.state !== "hit" && cache.state !== "stale") return null;

  const seconds = Math.max(1, Math.round(cache.ageMs / 1000));
  const title =
    `Cached: showing a value from ${seconds}s ago while the box recalculates it. ` +
    `It will update on its own in a moment.`;

  return (
    <span
      role="img"
      aria-label={title}
      title={title}
      className={cn(
        "inline-block size-1.5 shrink-0 rounded-full bg-amber-500 align-middle animate-pulse",
        className,
      )}
    />
  );
}
