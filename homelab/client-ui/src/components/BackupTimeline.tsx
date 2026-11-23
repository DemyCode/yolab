import { useEffect, useRef, useState } from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import { Select } from "@/components/ui/input";
import { RowAction } from "@/components/ui/list";
import { formatDateTime } from "@/lib/format";
import type { RestorePoint } from "@/lib/backups";
import {
  FRAMES,
  SCALES,
  ageHours,
  ageMarks,
  frameRange,
  spread,
  tickLabel,
  type Frame,
  type TimeScale,
} from "@/lib/timeline";

const HEIGHT = 120;
const LABEL_GAP_PX = 56;

function token(name: string, fallback: string): string {
  const value = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return value || fallback;
}

export function BackupTimeline({
  points,
  selected,
  now,
  onSelect,
}: {
  points: RestorePoint[];
  selected: string | null;
  now: number;
  onSelect: (id: string) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [frame, setFrame] = useState<Frame>("all");
  const [scale, setScale] = useState<TimeScale>("even");
  const [zoom, setZoom] = useState<[number, number] | null>(null);
  const selectedRef = useRef(selected);
  const plotRef = useRef<uPlot | null>(null);
  const pick = useRef(onSelect);

  useEffect(() => {
    pick.current = onSelect;
  }, [onSelect]);

  useEffect(() => {
    selectedRef.current = selected;
    plotRef.current?.redraw(false);
  }, [selected]);

  useEffect(() => {
    const el = box.current;
    if (!el || points.length === 0) return;
    const ordered = points
      .map((p) => ({ id: p.snapshot_id, age: ageHours(p.time, now) }))
      .sort((a, b) => a.age - b.age);
    const ages = ordered.map((p) => p.age);
    const [lo, hi] = zoom ?? frameRange(ages, frame, scale);
    const fg = token("--fg", "#111");
    const muted = token("--fg-subtle", "#888");
    const primary = token("--primary", "#4f46e5");
    const border = token("--border", "#ddd");
    let dragged = false;

    const opts: uPlot.Options = {
      width: el.clientWidth,
      height: HEIGHT,
      legend: { show: false },
      cursor: {
        y: false,
        points: { show: false },
        drag: { x: true, y: false, setScale: false },
      },
      scales: {
        x: {
          time: false,
          distr: scale === "even" ? 3 : 1,
          log: 10,
          range: () => [lo, hi],
        },
        y: { range: () => [0, 2] },
      },
      axes: [
        {
          stroke: muted,
          grid: { stroke: border, width: 1 },
          ticks: { stroke: border, width: 1 },
          filter: (_u, splits) => splits,
          ...(scale === "even" && {
            splits: (u: uPlot, _axis: number, min: number, max: number) =>
              spread(
                ageMarks(min, max),
                (h) => u.valToPos(h, "x"),
                LABEL_GAP_PX,
              ),
          }),
          values: (_u, splits) => splits.map(tickLabel),
        },
        { show: false },
      ],
      series: [
        {},
        {
          paths: () => null,
          points: { show: true, size: 9, fill: muted, stroke: muted },
        },
      ],
      hooks: {
        draw: [
          (u) => {
            const at = ordered.findIndex((p) => p.id === selectedRef.current);
            if (at < 0) return;
            const x = u.valToPos(ordered[at].age, "x", true);
            const y = u.valToPos(1, "y", true);
            const ctx = u.ctx;
            ctx.save();
            ctx.beginPath();
            ctx.arc(x, y, 9 * devicePixelRatio, 0, 2 * Math.PI);
            ctx.fillStyle = primary;
            ctx.fill();
            ctx.lineWidth = 3 * devicePixelRatio;
            ctx.strokeStyle = fg;
            ctx.globalAlpha = 0.25;
            ctx.stroke();
            ctx.restore();
          },
        ],
        setSelect: [
          (u) => {
            if (u.select.width < 4) return;
            const from = u.posToVal(u.select.left, "x");
            const to = u.posToVal(u.select.left + u.select.width, "x");
            u.setSelect({ left: 0, top: 0, width: 0, height: 0 }, false);
            dragged = true;
            setZoom([Math.max(from, 0), to]);
          },
        ],
      },
    };

    const plot = new uPlot(opts, [ages, ages.map(() => 1)], el);
    plotRef.current = plot;

    function click() {
      const idx = plot.cursor.idx;
      if (idx == null || dragged) return;
      pick.current(ordered[idx].id);
    }
    plot.over.addEventListener("click", click);
    plot.over.title = "Click a copy, or drag to look closer";

    const resize = new ResizeObserver(() => {
      plot.setSize({ width: el.clientWidth, height: HEIGHT });
    });
    resize.observe(el);

    return () => {
      resize.disconnect();
      plot.over.removeEventListener("click", click);
      plot.destroy();
      plotRef.current = null;
    };
  }, [points, now, frame, scale, zoom]);

  const oldest = points[points.length - 1];

  return (
    <div className="rounded-card border border-border p-4">
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <Select
          value={frame}
          onChange={(e) => {
            setZoom(null);
            setFrame(e.target.value as Frame);
          }}
          aria-label="Time shown"
          className="h-9 w-auto"
        >
          {FRAMES.map((f) => (
            <option key={f.value} value={f.value}>
              {f.label}
            </option>
          ))}
        </Select>
        <Select
          value={scale}
          onChange={(e) => setScale(e.target.value as TimeScale)}
          aria-label="How time is drawn"
          className="h-9 w-auto"
        >
          {SCALES.map((s) => (
            <option key={s.value} value={s.value}>
              {s.label}
            </option>
          ))}
        </Select>
        {zoom && (
          <RowAction onClick={() => setZoom(null)}>
            Show the whole frame
          </RowAction>
        )}
      </div>
      <div ref={box} className="w-full" />
      <div className="mt-1 flex justify-between font-mono text-xs tabular-nums text-fg-subtle">
        <span>Newest</span>
        <span>{oldest ? `Oldest · ${formatDateTime(oldest.time)}` : ""}</span>
      </div>
    </div>
  );
}
