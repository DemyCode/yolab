import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, getProgressive, isCached } from "./api";
import type { CacheMeta } from "./api";

export interface Resource<T> {
  data: T | undefined;
  loading: boolean;
  stale: boolean;
  error: string | null;
  refresh: () => Promise<void>;
  mutate: (updater: T | ((prev: T | undefined) => T)) => void;
  cache: CacheMeta | null;
  cached: boolean;
}

export function useResource<T>(
  key: string | null,
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
    let sawFrame = false;
    try {
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

export function useApi<T>(
  key: string | null,
  path: string,
  opts: { pollMs?: number } = {},
): Resource<T> {
  return useResource<T>(
    key,
    (onPartial) => getProgressive<T>(path, onPartial),
    opts,
  );
}
