import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "./api";

/**
 * Cached-first data fetching.
 *
 * The rule this exists to enforce: never show a spinner where a previous
 * answer exists. Opening the app on a phone over a home VPN can take a second
 * or two to reach the box, and a blank screen for that second is the single
 * biggest reason the UI feels slow — the data is late, but the *layout* did
 * not have to be.
 *
 * So: render last known values immediately from localStorage, refresh in the
 * background, and only ever show a skeleton on the very first visit. On error
 * we keep whatever we had and flag it stale rather than replacing content with
 * an error page.
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

function readCache<T>(key: string): T | undefined {
  try {
    const raw = localStorage.getItem(`yolab.cache.${key}`);
    return raw ? (JSON.parse(raw) as T) : undefined;
  } catch {
    return undefined;
  }
}

function writeCache<T>(key: string, value: T) {
  try {
    localStorage.setItem(`yolab.cache.${key}`, JSON.stringify(value));
  } catch {
    // Quota or private-mode failures are not worth surfacing: the cache is an
    // optimisation, and everything still works without it.
  }
}

export function useResource<T>(
  /** Stable cache key. Pass `null` to disable the fetch entirely. */
  key: string | null,
  fetcher: () => Promise<T>,
  opts: { pollMs?: number } = {},
): Resource<T> {
  const { pollMs } = opts;
  const [data, setData] = useState<T | undefined>(() =>
    key ? readCache<T>(key) : undefined,
  );
  const [loading, setLoading] = useState(() => (key ? !readCache(key) : false));
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
      writeCache(key, next);
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

  const mutate = useCallback(
    (updater: T | ((prev: T | undefined) => T)) => {
      setData((prev) => {
        const next =
          typeof updater === "function"
            ? (updater as (p: T | undefined) => T)(prev)
            : updater;
        if (key) writeCache(key, next);
        return next;
      });
    },
    [key],
  );

  return { data, loading, stale, error, refresh, mutate };
}
