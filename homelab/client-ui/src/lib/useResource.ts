import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, api } from "./api";
import { recall, remember } from "./localCache";
import { joinOrStart, startFresh } from "./inflight";

export interface Resource<T> {
  data: T | undefined;
  loading: boolean;
  stale: boolean;
  error: string | null;
  refresh: () => Promise<void>;
  mutate: (updater: T | ((prev: T | undefined) => T)) => void;
}

export interface ResourceOptions {
  pollMs?: number;
  persist?: boolean;
}

function recallIf<T>(key: string | null, persist: boolean) {
  return persist && key ? recall<T>(key) : null;
}

export function useResource<T>(
  key: string | null,
  fetcher: () => Promise<T>,
  opts: ResourceOptions = {},
): Resource<T> {
  const { pollMs, persist = false } = opts;
  const [initial] = useState(() => recallIf<T>(key, persist));
  const [data, setData] = useState<T | undefined>(initial?.data);
  const [loading, setLoading] = useState(() => Boolean(key) && !initial);
  const [stale, setStale] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const shownKey = useRef(key);

  useEffect(() => {
    if (shownKey.current === key) return;
    shownKey.current = key;
    const entry = recallIf<T>(key, persist);
    setData(entry?.data);
    setLoading(Boolean(key) && !entry);
    setStale(false);
    setError(null);
  }, [key, persist]);

  const fetcherRef = useRef(fetcher);
  useEffect(() => {
    fetcherRef.current = fetcher;
  });

  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const load = useCallback(
    async (fresh: boolean) => {
      if (!key) return;
      try {
        const run = () => fetcherRef.current();
        const next = await (fresh
          ? startFresh(key, run)
          : joinOrStart(key, run));
        if (!alive.current || shownKey.current !== key) return;
        if (persist) remember(key, next, Date.now());
        setData(next);
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
    },
    [key, persist],
  );

  const refresh = useCallback(() => load(true), [load]);
  const poll = useCallback(() => load(false), [load]);

  useEffect(() => {
    void poll();
    if (!pollMs) return;
    const id = setInterval(() => {
      if (document.visibilityState === "visible") void poll();
    }, pollMs);
    const onVisible = () => {
      if (document.visibilityState === "visible") void poll();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      clearInterval(id);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [poll, pollMs]);

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
  };
}

export function useApi<T>(
  key: string | null,
  path: string,
  opts: ResourceOptions = {},
): Resource<T> {
  return useResource<T>(key, () => api.get<T>(path), {
    persist: true,
    ...opts,
  });
}
