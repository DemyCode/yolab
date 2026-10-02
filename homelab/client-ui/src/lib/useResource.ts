import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, getProgressive, isCached } from "./api";
import type { CacheMeta } from "./api";
import { recall, remember } from "./localCache";
import type { Remembered } from "./localCache";

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

export function rememberedMeta(
  entry: Remembered<unknown>,
  now = Date.now(),
): CacheMeta {
  return { state: "stale", ageMs: Math.max(0, now - entry.savedAt), ttlMs: 0 };
}

export function frameSavedAt(meta: CacheMeta | null, now = Date.now()): number {
  return now - (meta?.ageMs ?? 0);
}

export function shouldShowFrame(
  meta: CacheMeta | null,
  savedAt: number,
  shownAt: number,
): boolean {
  return !isCached(meta) || savedAt >= shownAt;
}

export function useResource<T>(
  key: string | null,
  fetcher: (
    onPartial: (value: T, meta: CacheMeta | null) => void,
  ) => Promise<T>,
  opts: { pollMs?: number } = {},
): Resource<T> {
  const { pollMs } = opts;
  const [initial] = useState(() => (key ? recall<T>(key) : null));
  const [data, setData] = useState<T | undefined>(initial?.data);
  const [loading, setLoading] = useState(() => Boolean(key) && !initial);
  const [stale, setStale] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [cache, setCache] = useState<CacheMeta | null>(() =>
    initial ? rememberedMeta(initial) : null,
  );

  const shownAt = useRef(initial?.savedAt ?? -Infinity);
  const shownKey = useRef(key);

  useEffect(() => {
    if (shownKey.current === key) return;
    shownKey.current = key;
    const entry = key ? recall<T>(key) : null;
    shownAt.current = entry?.savedAt ?? -Infinity;
    setData(entry?.data);
    setCache(entry ? rememberedMeta(entry) : null);
    setLoading(Boolean(key) && !entry);
    setStale(false);
    setError(null);
  }, [key]);

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
    let called = false;
    try {
      const next = await fetcherRef.current((partial, meta) => {
        called = true;
        if (!alive.current || shownKey.current !== key) return;
        const savedAt = frameSavedAt(meta);
        if (!shouldShowFrame(meta, savedAt, shownAt.current)) return;
        shownAt.current = savedAt;
        if (meta) remember(key, partial, savedAt);
        setData(partial);
        setCache(meta);
        setStale(false);
        setError(null);
        setLoading(false);
      });
      if (!alive.current || shownKey.current !== key) return;
      if (!called) {
        shownAt.current = Date.now();
        setData(next);
        setCache(null);
      }
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
