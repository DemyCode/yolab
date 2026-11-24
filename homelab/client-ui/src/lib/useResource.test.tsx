import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "./api";
import { forget, recall, remember } from "./localCache";
import { useApi, useResource } from "./useResource";
import { useHealStatus } from "./heal";

let n = 0;
const freshKey = () => `use-resource-test-${++n}`;
const useKey = () => useState(freshKey)[0];

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function setVisibility(state: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", {
    value: state,
    configurable: true,
  });
}

beforeEach(() => setVisibility("visible"));
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("useResource", () => {
  it("is loading until the first answer arrives, then shows it", async () => {
    const d = deferred<string>();
    const { result } = renderHook(() => useResource(useKey(), () => d.promise));
    expect(result.current.loading).toBe(true);
    expect(result.current.data).toBeUndefined();

    await act(async () => d.resolve("hello"));
    expect(result.current).toMatchObject({
      data: "hello",
      loading: false,
      stale: false,
      error: null,
    });
  });

  it("asks for nothing and is not loading without a key", async () => {
    const fetcher = vi.fn(async () => "never");
    const { result } = renderHook(() => useResource(null, fetcher));
    await act(async () => {});
    expect(fetcher).not.toHaveBeenCalled();
    expect(result.current.loading).toBe(false);
    expect(result.current.data).toBeUndefined();
  });

  it("shows the remembered value at once when persisting, then the live one", async () => {
    const key = freshKey();
    remember(key, "remembered", Date.now());
    const d = deferred<string>();
    const { result } = renderHook(() =>
      useResource(key, () => d.promise, { persist: true }),
    );
    expect(result.current.data).toBe("remembered");
    expect(result.current.loading).toBe(false);

    await act(async () => d.resolve("live"));
    expect(result.current.data).toBe("live");
    expect(recall<string>(key)?.data).toBe("live");
    forget(key);
  });

  it("neither reads nor writes the remembered value unless asked to persist", async () => {
    const key = freshKey();
    remember(key, "remembered", Date.now());
    const { result } = renderHook(() => useResource(key, async () => "live"));
    expect(result.current.data).toBeUndefined();
    await waitFor(() => expect(result.current.data).toBe("live"));
    expect(recall<string>(key)?.data).toBe("remembered");
    forget(key);
  });

  it("keeps the last value and marks it stale when a fetch fails", async () => {
    let fail = false;
    const { result } = renderHook(() =>
      useResource(useKey(), async () => {
        if (fail) throw new Error("server is down");
        return "first";
      }),
    );
    await waitFor(() => expect(result.current.data).toBe("first"));

    fail = true;
    await act(() => result.current.refresh());
    expect(result.current).toMatchObject({
      data: "first",
      stale: true,
      error: "server is down",
      loading: false,
    });

    fail = false;
    await act(() => result.current.refresh());
    expect(result.current).toMatchObject({ stale: false, error: null });
  });

  it("says something went wrong when the failure carries no message", async () => {
    const { result } = renderHook(() =>
      useResource(useKey(), () => Promise.reject("not an Error")),
    );
    await waitFor(() =>
      expect(result.current.error).toBe("Something went wrong"),
    );
  });

  it("shows no error for an expired session, which sends the user to sign in instead", async () => {
    const { result } = renderHook(() =>
      useResource(useKey(), () =>
        Promise.reject(new ApiError(401, "Your session expired.")),
      ),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current).toMatchObject({ error: null, stale: false });
  });

  it("shows an error for any other refusal", async () => {
    const { result } = renderHook(() =>
      useResource(useKey(), () =>
        Promise.reject(new ApiError(403, "Not allowed")),
      ),
    );
    await waitFor(() => expect(result.current.error).toBe("Not allowed"));
  });

  it("starts over for a new key and drops the old key's late answer", async () => {
    const answers = new Map<string, ReturnType<typeof deferred<string>>>();
    const fetchFor = (key: string) => () => {
      const d = deferred<string>();
      answers.set(key, d);
      return d.promise;
    };
    const a = freshKey();
    const b = freshKey();
    const { result, rerender } = renderHook(
      ({ key }) => useResource(key, fetchFor(key)),
      { initialProps: { key: a } },
    );
    await waitFor(() => expect(answers.has(a)).toBe(true));

    rerender({ key: b });
    await waitFor(() => expect(answers.has(b)).toBe(true));
    expect(result.current.loading).toBe(true);

    await act(async () => answers.get(a)!.resolve("from a"));
    expect(result.current.data).toBeUndefined();

    await act(async () => answers.get(b)!.resolve("from b"));
    expect(result.current.data).toBe("from b");
    expect(result.current.loading).toBe(false);
  });

  it("shows the new key's remembered value when the key changes", async () => {
    const a = freshKey();
    const b = freshKey();
    remember(b, "b remembered", Date.now());
    const { result, rerender } = renderHook(
      ({ key }) =>
        useResource(key, () => new Promise<string>(() => {}), {
          persist: true,
        }),
      { initialProps: { key: a } },
    );
    expect(result.current.loading).toBe(true);
    rerender({ key: b });
    expect(result.current.data).toBe("b remembered");
    expect(result.current.loading).toBe(false);
    forget(b);
  });

  it("drops an answer, or a failure, that arrives after it is gone", async () => {
    const ok = deferred<string>();
    const bad = deferred<string>();
    const first = renderHook(() => useResource(useKey(), () => ok.promise));
    const second = renderHook(() => useResource(useKey(), () => bad.promise));
    first.unmount();
    second.unmount();
    await act(async () => {
      ok.resolve("late");
      bad.reject(new Error("late"));
    });
    expect(first.result.current.data).toBeUndefined();
    expect(second.result.current.error).toBeNull();
  });

  it("joins a fetch already in flight for the same key, but refresh starts a new one", async () => {
    const key = freshKey();
    const d = deferred<string>();
    const fetcher = vi.fn(() => d.promise);
    const { result } = renderHook(() => useResource(key, fetcher));
    renderHook(() => useResource(key, fetcher));
    expect(fetcher).toHaveBeenCalledTimes(1);

    let refreshed: Promise<void> = Promise.resolve();
    act(() => {
      refreshed = result.current.refresh();
    });
    expect(fetcher).toHaveBeenCalledTimes(2);
    await act(async () => {
      d.resolve("done");
      await refreshed;
    });
    expect(result.current.data).toBe("done");
  });

  it("uses the latest fetcher it was given", async () => {
    const key = freshKey();
    const { result, rerender } = renderHook(
      ({ value }) => useResource(key, async () => value),
      { initialProps: { value: "old" } },
    );
    await waitFor(() => expect(result.current.data).toBe("old"));
    rerender({ value: "new" });
    await act(() => result.current.refresh());
    expect(result.current.data).toBe("new");
  });

  it("polls while the page is visible and stops when it goes away", async () => {
    vi.useFakeTimers();
    let calls = 0;
    const { unmount } = renderHook(() =>
      useResource(useKey(), async () => ++calls, { pollMs: 1000 }),
    );
    await act(async () => {});
    expect(calls).toBe(1);

    await act(() => vi.advanceTimersByTimeAsync(1000));
    expect(calls).toBe(2);

    setVisibility("hidden");
    await act(() => vi.advanceTimersByTimeAsync(3000));
    expect(calls).toBe(2);

    setVisibility("visible");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(calls).toBe(3);

    unmount();
    await act(() => vi.advanceTimersByTimeAsync(3000));
    document.dispatchEvent(new Event("visibilitychange"));
    expect(calls).toBe(3);
  });

  it("does not fetch on a visibility change to hidden", async () => {
    let calls = 0;
    renderHook(() =>
      useResource(useKey(), async () => ++calls, { pollMs: 1000 }),
    );
    await waitFor(() => expect(calls).toBe(1));
    setVisibility("hidden");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(calls).toBe(1);
  });

  it("does not poll without an interval", async () => {
    vi.useFakeTimers();
    let calls = 0;
    renderHook(() => useResource(useKey(), async () => ++calls));
    await act(async () => {});
    await act(() => vi.advanceTimersByTimeAsync(60_000));
    expect(calls).toBe(1);
  });

  it("replaces the shown value with mutate, by value or from the previous one", async () => {
    const { result } = renderHook(() => useResource(useKey(), async () => 1));
    await waitFor(() => expect(result.current.data).toBe(1));
    act(() => result.current.mutate(5));
    expect(result.current.data).toBe(5);
    act(() => result.current.mutate((prev) => (prev ?? 0) + 1));
    expect(result.current.data).toBe(6);
  });
});

describe("useApi", () => {
  it("GETs the path and remembers the answer", async () => {
    const key = freshKey();
    const fetchMock = vi.fn(
      async () =>
        new Response(JSON.stringify({ ok: 1 }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useApi(key, "/api/thing"));
    await waitFor(() => expect(result.current.data).toEqual({ ok: 1 }));
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/thing",
      expect.objectContaining({ credentials: "include" }),
    );
    expect(recall(key)?.data).toEqual({ ok: 1 });
    forget(key);
  });

  it("can be told not to remember", async () => {
    const key = freshKey();
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify(2), { status: 200 })),
    );
    const { result } = renderHook(() =>
      useApi(key, "/api/thing", { persist: false }),
    );
    await waitFor(() => expect(result.current.data).toBe(2));
    expect(recall(key)).toBeNull();
  });
});

describe("useHealStatus", () => {
  it("reads /api/heal under the heal key", async () => {
    forget("heal");
    const fetchMock = vi.fn(
      async () => new Response(JSON.stringify({ survey: 1 }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useHealStatus(0));
    await waitFor(() => expect(result.current.data).toEqual({ survey: 1 }));
    expect(fetchMock).toHaveBeenCalledWith("/api/heal", expect.anything());
    expect(recall("heal")?.data).toEqual({ survey: 1 });
    forget("heal");
  });
});
