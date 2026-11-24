import { act, fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type uPlot from "uplot";
import { BackupTimeline } from "./BackupTimeline";
import type { RestorePoint } from "@/lib/backups";

interface Plot {
  opts: uPlot.Options;
  data: number[][];
  over: HTMLDivElement;
  cursor: { idx: number | null };
  select: { left: number; width: number };
  ctx: Record<string, unknown>;
  redraw: ReturnType<typeof vi.fn>;
  setSize: ReturnType<typeof vi.fn>;
  setSelect: ReturnType<typeof vi.fn>;
  destroy: ReturnType<typeof vi.fn>;
  valToPos: (v: number) => number;
  posToVal: (p: number) => number;
}

const plots: Plot[] = [];

vi.mock("uplot", () => ({
  default: class {
    constructor(opts: uPlot.Options, data: number[][]) {
      const plot: Plot = {
        opts,
        data,
        over: document.createElement("div"),
        cursor: { idx: null },
        select: { left: 0, width: 0 },
        ctx: {
          save: vi.fn(),
          beginPath: vi.fn(),
          arc: vi.fn(),
          fill: vi.fn(),
          stroke: vi.fn(),
          restore: vi.fn(),
        },
        redraw: vi.fn(),
        setSize: vi.fn(),
        setSelect: vi.fn(),
        destroy: vi.fn(),
        valToPos: (v) => v * 10,
        posToVal: (p) => p / 10,
      };
      plots.push(plot);
      return plot;
    }
  },
}));

let resized: (() => void) | null = null;
const disconnect = vi.fn();

beforeEach(() => {
  plots.length = 0;
  resized = null;
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(cb: () => void) {
        resized = cb;
      }
      observe() {}
      disconnect = disconnect;
    },
  );
});

const now = Date.parse("2026-10-01T12:00:00Z");
const point = (id: string, hoursAgo: number): RestorePoint => ({
  snapshot_id: id,
  time: new Date(now - hoursAgo * 3_600_000).toISOString(),
});
const points = [point("new", 1), point("mid", 24), point("old", 200)];

function timeline(over: Partial<Parameters<typeof BackupTimeline>[0]> = {}) {
  const onSelect = vi.fn();
  const view = render(
    <BackupTimeline
      points={points}
      selected={null}
      now={now}
      onSelect={onSelect}
      {...over}
    />,
  );
  return { onSelect, ...view };
}

const last = () => plots[plots.length - 1];

describe("BackupTimeline", () => {
  it("draws nothing without copies, and says nothing about the oldest", () => {
    timeline({ points: [] });
    expect(plots).toHaveLength(0);
    expect(screen.getByText("Newest").nextElementSibling!.textContent).toBe("");
  });

  it("plots every copy by age, newest first, on a spread-out scale", () => {
    timeline();
    const p = last();
    expect(p.data[0]).toEqual([1, 24, 200]);
    expect(p.data[1]).toEqual([1, 1, 1]);
    const x = p.opts.scales!.x!;
    expect(x.distr).toBe(3);
    const range = (x.range as unknown as () => [number, number])();
    expect(range[0]).toBeLessThan(1);
    expect((p.opts.scales!.y!.range as unknown as () => number[])()).toEqual([
      0, 2,
    ]);
    expect(screen.getByText(/^Oldest · /)).toBeTruthy();
    expect(p.over.title).toBe("Click a copy, or drag to look closer");
    const axis = p.opts.axes![0];
    const u = p as unknown as uPlot;
    expect(
      (axis.filter as (u: uPlot, s: number[]) => number[])(u, [1, 2]),
    ).toEqual([1, 2]);
    expect(
      (axis.values as (u: uPlot, s: number[]) => unknown[])(u, [24]),
    ).toEqual(["24 h"]);
    const splits = (
      axis.splits as (u: uPlot, a: number, lo: number, hi: number) => number[]
    )(u, 0, 1, 1000);
    expect(splits.length).toBeGreaterThan(0);
    expect((p.opts.series![1] as { paths: () => null }).paths()).toBeNull();
  });

  it("draws time evenly when asked, without spreading labels itself", () => {
    timeline();
    fireEvent.change(screen.getByLabelText("How time is drawn"), {
      target: { value: "true" },
    });
    expect(last().opts.scales!.x!.distr).toBe(1);
    expect(last().opts.axes![0].splits).toBeUndefined();
  });

  it("rings the chosen copy, and redraws when the choice changes", () => {
    const { rerender, onSelect } = timeline({ selected: "mid" });
    const p = last();
    const draw = p.opts.hooks!.draw![0] as (u: uPlot) => void;
    draw(p as unknown as uPlot);
    expect(p.ctx.arc).toHaveBeenCalledWith(
      240,
      10,
      expect.any(Number),
      0,
      2 * Math.PI,
    );
    expect(p.ctx.fillStyle).toBe("#4f46e5");
    rerender(
      <BackupTimeline
        points={points}
        selected="none"
        now={now}
        onSelect={onSelect}
      />,
    );
    expect(p.redraw).toHaveBeenCalledWith(false);
    (p.ctx.arc as ReturnType<typeof vi.fn>).mockClear();
    draw(p as unknown as uPlot);
    expect(p.ctx.arc).not.toHaveBeenCalled();
  });

  it("uses the theme's colours when they are set", () => {
    document.documentElement.style.setProperty("--primary", "#123456");
    try {
      timeline({ selected: "new" });
      const p = last();
      (p.opts.hooks!.draw![0] as (u: uPlot) => void)(p as unknown as uPlot);
      expect(p.ctx.fillStyle).toBe("#123456");
    } finally {
      document.documentElement.style.removeProperty("--primary");
    }
  });

  it("picks the copy that was clicked, but not after a drag or between copies", () => {
    const { onSelect, rerender } = timeline();
    const p = last();
    p.over.dispatchEvent(new MouseEvent("click"));
    expect(onSelect).not.toHaveBeenCalled();
    p.cursor.idx = 2;
    p.over.dispatchEvent(new MouseEvent("click"));
    expect(onSelect).toHaveBeenCalledWith("old");
    const later = vi.fn();
    rerender(
      <BackupTimeline
        points={points}
        selected={null}
        now={now}
        onSelect={later}
      />,
    );
    p.over.dispatchEvent(new MouseEvent("click"));
    expect(later).toHaveBeenCalledWith("old");
  });

  it("zooms in on a drag, ignores a tiny one, and shows the whole frame again", () => {
    timeline();
    const p = last();
    const setSelect = p.opts.hooks!.setSelect![0] as (u: uPlot) => void;
    p.select = { left: 10, width: 2 };
    act(() => setSelect(p as unknown as uPlot));
    expect(plots).toHaveLength(1);
    p.select = { left: -10, width: 300 };
    act(() => setSelect(p as unknown as uPlot));
    expect(p.setSelect).toHaveBeenCalledWith(
      { left: 0, top: 0, width: 0, height: 0 },
      false,
    );
    const zoomed = last();
    expect(zoomed).not.toBe(p);
    expect(
      (zoomed.opts.scales!.x!.range as unknown as () => number[])(),
    ).toEqual([0, 29]);
    zoomed.cursor.idx = 0;
    p.over.dispatchEvent(new MouseEvent("click"));
    fireEvent.click(
      screen.getByRole("button", { name: "Show the whole frame" }),
    );
    expect(
      screen.queryByRole("button", { name: "Show the whole frame" }),
    ).toBeNull();
  });

  it("a drag does not count as a click on that same chart", () => {
    const { onSelect } = timeline();
    const p = last();
    p.cursor.idx = 0;
    p.select = { left: 10, width: 300 };
    act(() =>
      (p.opts.hooks!.setSelect![0] as (u: uPlot) => void)(
        p as unknown as uPlot,
      ),
    );
    p.over.dispatchEvent(new MouseEvent("click"));
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("changing the time shown clears a zoom and redraws", () => {
    timeline();
    const p = last();
    p.select = { left: 10, width: 300 };
    act(() =>
      (p.opts.hooks!.setSelect![0] as (u: uPlot) => void)(
        p as unknown as uPlot,
      ),
    );
    fireEvent.change(screen.getByLabelText("Time shown"), {
      target: { value: "week" },
    });
    expect(
      screen.queryByRole("button", { name: "Show the whole frame" }),
    ).toBeNull();
    expect(p.destroy).toHaveBeenCalled();
  });

  it("follows the box's width and lets go of the chart when it goes away", () => {
    const { unmount } = timeline();
    const p = last();
    resized!();
    expect(p.setSize).toHaveBeenCalledWith({ width: 0, height: 120 });
    unmount();
    expect(disconnect).toHaveBeenCalled();
    expect(p.destroy).toHaveBeenCalled();
  });
});
