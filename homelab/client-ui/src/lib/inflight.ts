const pending = new Map<string, Promise<unknown>>();

export function startFresh<T>(key: string, run: () => Promise<T>): Promise<T> {
  const p: Promise<T> = run().finally(() => {
    if (pending.get(key) === p) pending.delete(key);
  });
  pending.set(key, p);
  return p;
}

export function joinOrStart<T>(key: string, run: () => Promise<T>): Promise<T> {
  const existing = pending.get(key);
  if (existing) return existing as Promise<T>;
  return startFresh(key, run);
}

export function isPending(key: string): boolean {
  return pending.has(key);
}
