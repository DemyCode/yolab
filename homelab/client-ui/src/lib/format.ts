export function formatBytes(bytes: number, digits = 1): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  const i = Math.min(
    units.length - 1,
    Math.floor(Math.log(bytes) / Math.log(1000)),
  );
  const value = bytes / Math.pow(1000, i);
  const shown =
    i === 0 || value >= 100 ? Math.round(value) : Number(value.toFixed(digits));
  return `${shown} ${units[i]}`;
}

export function formatDateTime(input: string | number | Date): string {
  return new Date(input).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function generateSecret(length = 24): string {
  const alphabet = "abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
  const bytes = new Uint32Array(length);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => alphabet[b % alphabet.length]).join("");
}

export function relativeTime(
  input: string | number | Date,
  now: Date = new Date(),
): string {
  const seconds = Math.round(
    (now.getTime() - new Date(input).getTime()) / 1000,
  );
  if (!Number.isFinite(seconds)) return "";
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60)
    return minutes === 1 ? "a minute ago" : `${minutes} minutes ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return hours === 1 ? "an hour ago" : `${hours} hours ago`;
  return formatDateTime(input);
}

export function roughDuration(ms: number): string {
  const minutes = Math.round(ms / 60_000);
  if (!Number.isFinite(minutes) || ms < 45_000) return "a few seconds";
  if (minutes <= 1) return "a minute";
  if (minutes < 60) return `${minutes} minutes`;
  const hours = Math.round(minutes / 60);
  return hours === 1 ? "an hour" : `${hours} hours`;
}

export function formatEuros(cents: number): string {
  const sign = cents < 0 ? "−" : "";
  return `${sign}€${(Math.abs(cents) / 100).toFixed(2)}`;
}

export function creditStatus(credit: {
  credit_cents: number;
  suspended: boolean;
  stops_on: string | null;
}): { text: string; tone?: "warn" | "error" } {
  const amount = formatEuros(credit.credit_cents);
  if (credit.suspended)
    return {
      text: `${amount} — YoLab tunnels and backups are stopped until you top up`,
      tone: "error",
    };
  if (credit.stops_on) {
    const day = new Date(`${credit.stops_on}T00:00:00Z`).toLocaleDateString(
      "en-GB",
      { day: "numeric", month: "long", year: "numeric", timeZone: "UTC" },
    );
    return {
      text: `${amount} — YoLab tunnels and backups will stop working at the end of ${day}`,
      tone: "warn",
    };
  }
  return { text: `${amount} credit left` };
}
