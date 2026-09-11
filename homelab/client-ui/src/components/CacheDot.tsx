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
 * A DOT AND NOTHING ELSE. It briefly carried its age as text ("cached 2s ago")
 * and that was wrong twice over: on the Storage page it appears for a few
 * seconds out of every twenty-second poll, so a line of text there flickers in
 * and out of the layout and reads as noise — and the age is not information
 * anyone acts on at this scale. Two seconds or five, the answer is the same:
 * wait a moment. The exact age is still in the tooltip for the rare case where
 * it matters.
 *
 * It renders NOTHING once the value is live, which is a second or a few after
 * the page opens. An indicator that is always on screen is one nobody reads.
 *
 * Deliberately does not tick upward while the refresh runs: that needs a clock
 * read during render, which is impure and rightly forbidden by the lint rules
 * here, and it would buy nothing for a thing that lives a few seconds.
 */
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
      // No text, so the dot itself has to carry the accessible name — a
      // colour-only signal is invisible to a screen reader.
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
