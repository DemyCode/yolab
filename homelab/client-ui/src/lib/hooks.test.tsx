import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useNow, useNowSecs } from "./useNow";
import { useRestorePlan } from "./useRestorePlan";
import { useRestorePoints } from "./useRestorePoints";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function answer(body: unknown, status = 200) {
  return vi.fn(
    async () =>
      new Response(typeof body === "string" ? body : JSON.stringify(body), {
        status,
      }),
  );
}

describe("useNow", () => {
  it("ticks at its interval and stops when unmounted", () => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    const { result, unmount } = renderHook(() => useNow(1000));
    expect(result.current).toBe(1_000_000);
    act(() => vi.advanceTimersByTime(1000));
    expect(result.current).toBe(1_001_000);
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("counts in seconds once a minute by default", () => {
    vi.useFakeTimers();
    vi.setSystemTime(120_000);
    const { result } = renderHook(() => useNowSecs());
    expect(result.current).toBe(120);
    act(() => vi.advanceTimersByTime(59_999));
    expect(result.current).toBe(120);
    act(() => vi.advanceTimersByTime(1));
    expect(result.current).toBe(180);
  });
});

describe("useRestorePlan", () => {
  it("asks for nothing without a service or a restore point", () => {
    const fetchMock = answer({});
    vi.stubGlobal("fetch", fetchMock);
    renderHook(() => useRestorePlan(null, "s1"));
    renderHook(() => useRestorePlan("yolab-a", ""));
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("reads the plan of the chosen point", async () => {
    const fetchMock = answer({ volumes: [] });
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useRestorePlan("yolab a", "s/1"));
    expect(result.current).toEqual({ plan: null, error: null });
    await waitFor(() => expect(result.current.plan).toEqual({ volumes: [] }));
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/backups/apps/yolab%20a/plan?snapshot_id=s%2F1",
      expect.anything(),
    );
  });

  it("says why the plan could not be read", async () => {
    vi.stubGlobal("fetch", answer('{"error":"gone"}', 404));
    const { result } = renderHook(() => useRestorePlan("a", "s"));
    await waitFor(() =>
      expect(result.current).toEqual({ plan: null, error: "gone" }),
    );
  });

  it("a failure that is not an error still says the backup could not be read", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        status: 200,
        ok: true,
        json: () => Promise.reject("not json"),
      })),
    );
    const { result } = renderHook(() => useRestorePlan("a", "s"));
    await waitFor(() =>
      expect(result.current.error).toBe("The backup could not be read."),
    );
  });

  it("drops an answer for a point no longer chosen", async () => {
    let release!: (r: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((r) => {
            release = r;
          }),
      ),
    );
    const { result, rerender } = renderHook(
      ({ snap }) => useRestorePlan("a", snap),
      { initialProps: { snap: "s1" } },
    );
    const first = release;
    rerender({ snap: "s2" });
    const second = release;
    await act(async () => {
      first(new Response(JSON.stringify({ which: 1 })));
      second(new Response(JSON.stringify({ which: 2 })));
    });
    await waitFor(() => expect(result.current.plan).toEqual({ which: 2 }));
  });

  it("drops a failure for a point no longer chosen", async () => {
    let fail!: (e: Error) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((_, reject) => {
            fail = reject;
          }),
      ),
    );
    const { result, unmount } = renderHook(() => useRestorePlan("a", "s"));
    unmount();
    await act(async () => fail(new Error("late")));
    expect(result.current.error).toBeNull();
  });
});

describe("useRestorePoints", () => {
  it("asks for nothing without a service", () => {
    const fetchMock = answer({});
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useRestorePoints(null));
    expect(fetchMock).not.toHaveBeenCalled();
    expect(result.current).toEqual({ points: null, error: null });
  });

  it("lists the service's points newest first", async () => {
    const fetchMock = answer({
      points: [
        { snapshot_id: "old", time: "2026-01-01T00:00:00Z" },
        { snapshot_id: "new", time: "2026-02-01T00:00:00Z" },
      ],
    });
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useRestorePoints("yolab a"));
    await waitFor(() => expect(result.current.points).not.toBeNull());
    expect(result.current.points?.map((p) => p.snapshot_id)).toEqual([
      "new",
      "old",
    ]);
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/backups/apps/yolab%20a/points",
      expect.anything(),
    );
  });

  it("a service without points has none", async () => {
    vi.stubGlobal("fetch", answer({}));
    const { result } = renderHook(() => useRestorePoints("a"));
    await waitFor(() =>
      expect(result.current).toEqual({ points: [], error: null }),
    );
  });

  it("says why the points could not be read", async () => {
    vi.stubGlobal("fetch", answer('{"error":"locked"}', 500));
    const { result } = renderHook(() => useRestorePoints("a"));
    await waitFor(() =>
      expect(result.current).toEqual({ points: [], error: "locked" }),
    );
  });

  it("a failure that is not an error still says the backups could not be read", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        status: 200,
        ok: true,
        json: () => Promise.reject("not json"),
      })),
    );
    const { result } = renderHook(() => useRestorePoints("a"));
    await waitFor(() =>
      expect(result.current.error).toBe("Your backups could not be read."),
    );
  });

  it("drops answers that arrive after the service changed", async () => {
    const pending: ((r: Response) => void)[] = [];
    const failing: ((e: Error) => void)[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((resolve, reject) => {
            pending.push(resolve);
            failing.push(reject);
          }),
      ),
    );
    const { result, rerender } = renderHook(({ ns }) => useRestorePoints(ns), {
      initialProps: { ns: "a" as string | null },
    });
    rerender({ ns: "b" });
    rerender({ ns: "c" });
    await act(async () => {
      pending[0](new Response(JSON.stringify({ points: [] })));
      failing[1](new Error("late"));
    });
    expect(result.current).toEqual({ points: null, error: null });
  });
});
