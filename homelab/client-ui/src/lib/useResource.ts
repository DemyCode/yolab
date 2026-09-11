import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, getProgressive, isCached } from "./api";
import type { CacheMeta } from "./api";

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
  /**
   * What the server said about the body currently in `data`, or null for a
   * resource that is not cached server-side.
   *
   * THE UI IS EXPECTED TO SHOW THIS. Rendering a remembered value as though it
   * were live is precisely the bug that got localStorage caching deleted from
   * this file; the server-side cache is only safe while the page keeps saying
   * which one it is holding.
   */
  cache: CacheMeta | null;
  /** Shorthand: the body on screen was remembered, not just computed. */
  cached: boolean;
}

export function useResource<T>(
  /** Stable cache key. Pass `null` to disable the fetch entirely. */
  key: string | null,
  /**
   * Receives an `onPartial` callback. A plain fetcher ignores it and resolves
   * once, exactly as before; a progressive one (`api.getProgressive`) calls it
   * for the remembered value and then resolves with the freshly computed one,
   * so the page paints twice from a single request.
   */
  fetcher: (
    onPartial: (value: T, meta: CacheMeta | null) => void,
  ) => Promise<T>,
  opts: { pollMs?: number } = {},
): Resource<T> {
  const { pollMs } = opts;
  const [data, setData] = useState<T | undefined>(undefined);
  const [loading, setLoading] = useState(() => Boolean(key));
  const [stale, setStale] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [cache, setCache] = useState<CacheMeta | null>(null);

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
    // A plain fetcher never calls onPartial, so without this the meta from a
    // previous progressive cycle would stick to a body it does not describe.
    let sawFrame = false;
    try {
      // Frame one lands here: show it at once, labelled, rather than holding a
      // spinner until the box has finished shelling out for the real value.
      const next = await fetcherRef.current((partial, meta) => {
        if (!alive.current) return;
        sawFrame = true;
        setData(partial);
        setCache(meta);
        setStale(false);
        setError(null);
        setLoading(false);
      });
      if (!alive.current) return;
      setData(next);
      if (!sawFrame) setCache(null);
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

  return {
    data,
    loading,
    stale,
    error,
    refresh,
    mutate,
    cache,
    cached: isCached(cache),
  };
}

/**
 * `useResource` for a plain GET, fetched progressively.
 *
 * The one-line form of what every call site was writing by hand:
 *
 *   useResource<T>("key", () => api.get("/api/x"))          becomes
 *   useApi<T>("key", "/api/x")
 *
 * and it comes back two-phase — the remembered value paints immediately, the
 * real one replaces it when the box has finished computing it. Routes the
 * server does not cache answer with ordinary JSON and `cache` stays null, so
 * call sites need no knowledge of which routes those are.
 *
 * Use `useResource` directly only when the fetch is not a plain GET of one path
 * — a derived or composed request, or one that has to post something first.
 */
export function useApi<T>(
  key: string | null,
  path: string,
  opts: { pollMs?: number } = {},
): Resource<T> {
  // `path` is in the dependency list via the key the caller passes; a changing
  // path with a fixed key would be a bug at the call site, not here.
  return useResource<T>(
    key,
    (onPartial) => getProgressive<T>(path, onPartial),
    opts,
  );
}
