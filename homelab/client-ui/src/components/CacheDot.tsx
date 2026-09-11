import type { CacheMeta } from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * A small amber dot marking a value that was remembered rather than just
 * measured.
 *
 * THIS IS A SAFETY CONTROL, NOT DECORATION.
 *
 * `useResource` used to render last-known values from localStorage and that was
 * deliberately deleted, because during an incident a confidently-rendered stale
 * number — "1.2 TB free" from before a disk failed, a disk still shown "in use"
 * after it was pulled — is worse than a spinner. Caching is back, server-side,
 * and it is only acceptable because of this: the page shows a remembered value
 * instantly, marked, and the real one replaces it a moment later.
 *
 * So it renders NOTHING once the value is live, which is within a second or a
 * few of the page opening. An indicator that is always on screen is one nobody
 * reads; this appears exactly when the distinction matters and vanishes when it
 * stops. That is also why it is a dot and not a sentence — it sits beside a
 * heading or a number without pushing the layout around, and the age is on
 * hover for anyone who wants it.
 *
 * Deliberately does NOT tick upward while the refresh runs: that needs a clock
 * read during render, which is impure and rightly forbidden by the lint rules
 * here, and it would buy nothing because the dot's whole lifetime is the gap
 * between two frames of one request.
 */
export function CacheDot({
  cache,
  className,
  withAge = false,
}: {
  cache: CacheMeta | null;
  className?: string;
  /** Also print "Ns ago", where there is room for it. */
  withAge?: boolean;
}) {
  if (cache === null) return null;
  if (cache.state !== "hit" && cache.state !== "stale") return null;

  const seconds = Math.max(1, Math.round(cache.ageMs / 1000));
  const title = `Showing a value from ${seconds}s ago while the box recalculates it.`;

  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center gap-1.5 align-middle text-xs text-fg-muted",
        className,
      )}
      title={title}
    >
      <span
        // Not aria-hidden when it is the only thing rendered: a colour-only
        // signal with no accessible name is invisible to a screen reader.
        role="img"
        aria-label={title}
        className="size-1.5 rounded-full bg-amber-500 animate-pulse"
      />
      {withAge && <span aria-hidden>{seconds}s ago</span>}
    </span>
  );
}
