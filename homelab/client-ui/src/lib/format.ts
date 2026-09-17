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

/** "12 Mar 2026, 14:30" — a full timestamp, for a backup or restore record. */
export function formatDateTime(input: string | number | Date): string {
  return new Date(input).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
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
