const PREFIX = "yolab:cache:v1:";
export const MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;

export interface Remembered<T> {
  data: T;
  savedAt: number;
}

const memory = new Map<string, Remembered<unknown>>();

function storage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

export function remember<T>(key: string, data: T, savedAt: number): void {
  const prev = memory.get(key);
  if (prev && prev.savedAt > savedAt) return;
  const entry: Remembered<T> = { data, savedAt };
  memory.set(key, entry);
  try {
    storage()?.setItem(PREFIX + key, JSON.stringify(entry));
  } catch {
    forget(key);
    memory.set(key, entry);
  }
}

export function recall<T>(key: string, now = Date.now()): Remembered<T> | null {
  let entry = memory.get(key) as Remembered<T> | undefined;
  if (!entry) {
    try {
      const raw = storage()?.getItem(PREFIX + key);
      if (raw) {
        const parsed = JSON.parse(raw) as Remembered<T>;
        if (typeof parsed?.savedAt === "number" && "data" in parsed) {
          entry = parsed;
          memory.set(key, parsed);
        }
      }
    } catch {
      entry = undefined;
    }
  }
  if (!entry) return null;
  if (now - entry.savedAt > MAX_AGE_MS) {
    forget(key);
    return null;
  }
  return entry;
}

export function forget(key: string): void {
  memory.delete(key);
  try {
    storage()?.removeItem(PREFIX + key);
  } catch {
    return;
  }
}

export function forgetAll(): void {
  memory.clear();
  const s = storage();
  if (!s) return;
  try {
    const keys: string[] = [];
    for (let i = 0; i < s.length; i++) {
      const k = s.key(i);
      if (k?.startsWith(PREFIX)) keys.push(k);
    }
    keys.forEach((k) => s.removeItem(k));
  } catch {
    return;
  }
}
