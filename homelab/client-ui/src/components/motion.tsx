import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";
const MOVE_MS = 320;
const ENTER_MS = 260;
const EXIT_MS = 200;
const ROLL_MS = 700;

export function prefersReducedMotion(): boolean {
  try {
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  } catch {
    return true;
  }
}

function canAnimate(el: HTMLElement | null | undefined): el is HTMLElement {
  return !!el && typeof el.animate === "function" && !prefersReducedMotion();
}

export interface Row<T> {
  key: string;
  item: T;
  leaving: boolean;
}

export function mergeRows<T>(
  prev: Row<T>[],
  items: T[],
  keyOf: (item: T) => string,
): Row<T>[] {
  const next: Row<T>[] = items.map((item) => ({
    key: keyOf(item),
    item,
    leaving: false,
  }));
  const kept = new Set(next.map((r) => r.key));
  prev.forEach((row, i) => {
    if (kept.has(row.key)) return;
    next.splice(Math.min(i, next.length), 0, { ...row, leaving: true });
    kept.add(row.key);
  });
  return next;
}

export function AnimatedList<T>({
  items,
  keyOf,
  children,
  itemClassName,
}: {
  items: T[];
  keyOf: (item: T) => string;
  children: (item: T) => ReactNode;
  itemClassName?: string;
}) {
  const [rows, setRows] = useState<Row<T>[]>(() =>
    mergeRows([], items, keyOf),
  );
  const nodes = useRef(new Map<string, HTMLElement>());
  const before = useRef(new Map<string, DOMRect>());
  const seen = useRef(new Set(rows.map((r) => r.key)));
  const exiting = useRef(new Set<string>());
  const keyOfRef = useRef(keyOf);
  keyOfRef.current = keyOf;

  function snapshot() {
    before.current = new Map();
    nodes.current.forEach((el, key) => {
      before.current.set(key, el.getBoundingClientRect());
    });
  }

  useEffect(() => {
    snapshot();
    setRows((prev) => mergeRows(prev, items, keyOfRef.current));
  }, [items]);

  useLayoutEffect(() => {
    rows.forEach((row) => {
      const el = nodes.current.get(row.key);
      if (row.leaving) {
        if (exiting.current.has(row.key)) return;
        exiting.current.add(row.key);
        const drop = () => {
          exiting.current.delete(row.key);
          snapshot();
          setRows((prev) => prev.filter((r) => !(r.key === row.key && r.leaving)));
        };
        if (!canAnimate(el)) return drop();
        el.style.pointerEvents = "none";
        el.animate(
          [
            { opacity: 1, transform: "scale(1)" },
            { opacity: 0, transform: "scale(0.96)" },
          ],
          { duration: EXIT_MS, easing: EASE, fill: "forwards" },
        ).onfinish = drop;
        return;
      }

      if (!seen.current.has(row.key)) {
        seen.current.add(row.key);
        if (canAnimate(el)) {
          el.animate(
            [
              { opacity: 0, transform: "translateY(6px) scale(0.98)" },
              { opacity: 1, transform: "none" },
            ],
            { duration: ENTER_MS, easing: EASE },
          );
        }
        return;
      }

      const was = before.current.get(row.key);
      if (!was || !canAnimate(el)) return;
      const now = el.getBoundingClientRect();
      const dx = was.left - now.left;
      const dy = was.top - now.top;
      if (Math.abs(dx) < 1 && Math.abs(dy) < 1) return;
      el.animate(
        [{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "none" }],
        { duration: MOVE_MS, easing: EASE },
      );
    });
    rows.forEach((row) => {
      if (row.leaving || !exiting.current.has(row.key)) return;
      exiting.current.delete(row.key);
      const el = nodes.current.get(row.key);
      if (!el) return;
      el.style.pointerEvents = "";
      el.getAnimations?.().forEach((a) => a.cancel());
    });
    before.current = new Map();
  }, [rows]);

  return (
    <>
      {rows.map((row) => (
        <div
          key={row.key}
          data-leaving={row.leaving || undefined}
          aria-hidden={row.leaving || undefined}
          className={cn(itemClassName)}
          ref={(el) => {
            if (el) nodes.current.set(row.key, el);
            else nodes.current.delete(row.key);
          }}
        >
          {children(row.item)}
        </div>
      ))}
    </>
  );
}

export function rollFormat(value: number): (n: number) => string {
  const decimals = Math.min(2, (String(value).split(".")[1] ?? "").length);
  return (n) => n.toFixed(decimals);
}

export function easeOut(t: number): number {
  return 1 - Math.pow(1 - Math.min(1, Math.max(0, t)), 3);
}

export function RollingNumber({
  value,
  format,
  className,
}: {
  value: number;
  format?: (n: number) => string;
  className?: string;
}) {
  const [shown, setShown] = useState(value);
  const shownRef = useRef(value);

  useEffect(() => {
    const from = shownRef.current;
    if (from === value) return;
    const set = (n: number) => {
      shownRef.current = n;
      setShown(n);
    };
    if (prefersReducedMotion() || typeof requestAnimationFrame !== "function") {
      set(value);
      return;
    }
    const start = performance.now();
    let frame = 0;
    const tick = (now: number) => {
      const t = (now - start) / ROLL_MS;
      if (t >= 1) return set(value);
      set(from + (value - from) * easeOut(t));
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [value]);

  const fmt = format ?? rollFormat(value);
  return <span className={cn("tabular-nums", className)}>{fmt(shown)}</span>;
}
