import { createElement, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";
import {
  easeOut,
  mergeRows,
  prefersReducedMotion,
  rollFormat,
} from "@/lib/motion";
import type { Row } from "@/lib/motion";

const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";
const MOVE_MS = 320;
const ENTER_MS = 260;
const EXIT_MS = 200;
const ROLL_MS = 700;

function canAnimate(el: HTMLElement | null | undefined): el is HTMLElement {
  return !!el && typeof el.animate === "function" && !prefersReducedMotion();
}

export function AnimatedList<T>({
  items,
  keyOf,
  children,
  itemClassName,
  as = "div",
}: {
  items: T[];
  keyOf: (item: T) => string;
  children: (item: T) => ReactNode;
  itemClassName?: string;
  as?: "div" | "li" | "tr";
}) {
  const [rows, setRows] = useState<Row<T>[]>(() => mergeRows([], items, keyOf));
  const nodes = useRef(new Map<string, HTMLElement>());
  const before = useRef(new Map<string, DOMRect>());
  const seen = useRef(new Set(rows.map((r) => r.key)));
  const exiting = useRef(new Set<string>());
  const keyOfRef = useRef(keyOf);
  useEffect(() => {
    keyOfRef.current = keyOf;
  });

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
          setRows((prev) =>
            prev.filter((r) => !(r.key === row.key && r.leaving)),
          );
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
      {rows.map((row) =>
        createElement(
          as,
          {
            key: row.key,
            "data-leaving": row.leaving || undefined,
            "aria-hidden": row.leaving || undefined,
            className: cn(itemClassName),
            ref: (el: HTMLElement | null) => {
              if (el) nodes.current.set(row.key, el);
              else nodes.current.delete(row.key);
            },
          },
          children(row.item),
        ),
      )}
    </>
  );
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

export function Collapse({
  open,
  children,
  className,
}: {
  open: boolean;
  children: ReactNode;
  className?: string;
}) {
  const [present, setPresent] = useState(open);
  const [last, setLast] = useState<ReactNode>(open ? children : null);
  const ref = useRef<HTMLDivElement>(null);
  const settled = useRef(false);

  useEffect(() => {
    if (open) setPresent(true);
  }, [open]);

  useEffect(() => {
    if (open) setLast(children);
  }, [open, children]);

  useLayoutEffect(() => {
    if (!settled.current) {
      settled.current = true;
      return;
    }
    const el = ref.current;
    if (!el) return;
    if (!canAnimate(el)) {
      if (!open) setPresent(false);
      return;
    }
    el.getAnimations().forEach((a) => a.cancel());
    const full = { height: `${el.scrollHeight}px`, opacity: 1 };
    const none = { height: "0px", opacity: 0 };
    el.style.overflow = "hidden";
    const anim = el.animate(open ? [none, full] : [full, none], {
      duration: ENTER_MS,
      easing: EASE,
    });
    anim.onfinish = () => {
      el.style.overflow = "";
      if (!open) setPresent(false);
    };
  }, [open, present]);

  if (!present) return null;
  return (
    <div ref={ref} className={cn("flow-root", className)}>
      {open ? children : last}
    </div>
  );
}

export function Swap({
  id,
  children,
  className,
}: {
  id: string | number;
  children: ReactNode;
  className?: string;
}) {
  const ref = useRef<HTMLSpanElement>(null);
  const prev = useRef(id);

  useLayoutEffect(() => {
    if (prev.current === id) return;
    prev.current = id;
    const el = ref.current;
    if (!canAnimate(el)) return;
    el.animate(
      [
        { opacity: 0, transform: "translateY(3px)" },
        { opacity: 1, transform: "none" },
      ],
      { duration: ENTER_MS, easing: EASE },
    );
  }, [id]);

  return (
    <span ref={ref} className={className ?? "inline-block"}>
      {children}
    </span>
  );
}
