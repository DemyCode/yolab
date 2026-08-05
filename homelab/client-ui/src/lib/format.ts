// Formatting helpers, all of which exist to keep machine units out of the UI.

/** "1.4 TB", "312 GB" — decimal units, because that is what disks are sold in. */
export function formatBytes(bytes: number, digits = 1): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  const i = Math.min(
    units.length - 1,
    Math.floor(Math.log(bytes) / Math.log(1000)),
  );
  const value = bytes / Math.pow(1000, i);
  // No "1.0 GB" — a trailing .0 reads as spurious precision.
  const shown =
    i === 0 || value >= 100 ? Math.round(value) : value.toFixed(digits);
  return `${shown} ${units[i]}`;
}

/** "just now", "3 hours ago", "yesterday", "12 Mar". */
export function formatRelative(input: string | number | Date): string {
  const then = new Date(input).getTime();
  if (Number.isNaN(then)) return "unknown";
  const seconds = Math.round((Date.now() - then) / 1000);

  if (seconds < 45) return "just now";
  if (seconds < 90) return "a minute ago";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} minutes ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return hours === 1 ? "an hour ago" : `${hours} hours ago`;
  const days = Math.round(hours / 24);
  if (days === 1) return "yesterday";
  if (days < 7) return `${days} days ago`;
  return new Date(then).toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
  });
}

/**
 * Kubernetes quantities ("50Gi", "200Gi") as something a person would say.
 * The install form collects these, and "50Gi" is jargon for "50 GB".
 */
export function formatQuantity(q: string): string {
  const m = /^(\d+(?:\.\d+)?)\s*([KMGTP]i?)?B?$/.exec(q.trim());
  if (!m) return q;
  const [, num, unit] = m;
  const suffix: Record<string, string> = {
    Ki: "KB",
    Mi: "MB",
    Gi: "GB",
    Ti: "TB",
    Pi: "PB",
    K: "KB",
    M: "MB",
    G: "GB",
    T: "TB",
    P: "PB",
  };
  return unit ? `${num} ${suffix[unit] ?? unit}` : `${num} B`;
}

/**
 * A password we generate so the person never has to invent one.
 *
 * Deliberately excludes characters that are ambiguous when read off a screen
 * and retyped on a phone (0/O, 1/l/I) — these get copied by hand more often
 * than anyone plans for.
 */
export function generateSecret(length = 24): string {
  const alphabet = "abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
  const bytes = new Uint32Array(length);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => alphabet[b % alphabet.length]).join("");
}
