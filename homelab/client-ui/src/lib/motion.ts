export function prefersReducedMotion(): boolean {
  try {
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  } catch {
    return true;
  }
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

export function rollFormat(value: number): (n: number) => string {
  const decimals = Math.min(2, (String(value).split(".")[1] ?? "").length);
  return (n) => n.toFixed(decimals);
}

export function easeOut(t: number): number {
  return 1 - Math.pow(1 - Math.min(1, Math.max(0, t)), 3);
}
