import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "./api";

/**
 * Fetch-on-mount, poll-while-visible data fetching.
 *
 * This used to also render last-known values from localStorage immediately,
 * to avoid a spinner on repeat visits. Dropped: during a real incident, the
 * box's actual state can change (or become inaccessible) between visits, and
 * a confidently-rendered stale number — "1.2 TB free" from before a disk
 * failed, a disk still shown "in use" after it was pulled — is worse than a
 * brief loading state. Every mount now always asks the backend and shows
 * nothing else until it answers.
 */
export interface Resource<T> {
  data: T | undefined;
  /** True only when there is nothing at all to show yet. */
  loading: boolean;
  /** We have data, but the last refresh failed — show it dimmed, not gone. */
  stale: boolean;
  error: string | null;
  refresh: () => Promise<void>;
  /** Apply a local change now; the next refresh confirms it. */
  mutate: (updater: T | ((prev: T | undefined) => T)) => void;
}

export function useResource<T>(
  /** Stable cache key. Pass `null` to disable the fetch entirely. */
  key: string | null,
  fetcher: () => Promise<T>,
  opts: { pollMs?: number } = {},
): Resource<T> {
  const { pollMs } = opts;
  const [data, setData] = useState<T | undefined>(undefined);
  const [loading, setLoading] = useState(() => Boolean(key));
  const [stale, setStale] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Keeping the fetcher in a ref lets callers pass an inline closure without
  // restarting the poll on every render.
  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const refresh = useCallback(async () => {
    if (!key) return;
    try {
      const next = await fetcherRef.current();
      if (!alive.current) return;
      setData(next);
      setStale(false);
      setError(null);
    } catch (e) {
      if (!alive.current) return;
      // A 401 is handled globally (back to sign-in); anything else means we
      // keep showing what we have.
      if (!(e instanceof ApiError && e.isUnauthorized)) {
        setStale(true);
        setError(e instanceof Error ? e.message : "Something went wrong");
      }
    } finally {
      if (alive.current) setLoading(false);
    }
  }, [key]);

  useEffect(() => {
    void refresh();
    if (!pollMs) return;
    // Polling while the tab is hidden burns the user's battery and the box's
    // CPU for output nobody is looking at.
    const id = setInterval(() => {
      if (document.visibilityState === "visible") void refresh();
    }, pollMs);
    const onVisible = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      clearInterval(id);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [refresh, pollMs]);

  const mutate = useCallback((updater: T | ((prev: T | undefined) => T)) => {
    setData((prev) =>
      typeof updater === "function"
        ? (updater as (p: T | undefined) => T)(prev)
        : updater,
    );
  }, []);

  return { data, loading, stale, error, refresh, mutate };
}
