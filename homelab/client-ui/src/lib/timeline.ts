export type TimeScale = "even" | "true";

export type Frame = "all" | "week" | "month" | "year";

interface FrameOption {
  value: Frame;
  label: string;
  hours: number | null;
}

export const FRAMES: FrameOption[] = [
  { value: "all", label: "All copies", hours: null },
  { value: "week", label: "Last 7 days", hours: 24 * 7 },
  { value: "month", label: "Last 30 days", hours: 24 * 30 },
  { value: "year", label: "Last year", hours: 24 * 365 },
];

export const SCALES: { value: TimeScale; label: string }[] = [
  { value: "even", label: "Recent days wider" },
  { value: "true", label: "Real time" },
];

export const NEAREST_HOURS = 0.05;

const HOUR_MS = 3_600_000;

export function ageHours(iso: string, now: number): number {
  return Math.max(NEAREST_HOURS, (now - new Date(iso).getTime()) / HOUR_MS);
}

export function frameRange(
  ages: number[],
  frame: Frame,
  scale: TimeScale,
): [number, number] {
  const limit = FRAMES.find((f) => f.value === frame)?.hours ?? null;
  const inFrame = ages.filter((a) => limit === null || a <= limit);
  const framed = limit !== null && inFrame.length > 0;
  const shown = framed ? inFrame : ages;
  const lo = Math.max(NEAREST_HOURS, Math.min(...shown));
  const hi = Math.max(lo, framed ? limit : Math.max(...shown));
  if (scale === "even") {
    return [lo / 1.3, hi * 1.3];
  }
  const pad = Math.max((hi - lo) * 0.04, 0.5);
  return [Math.max(0, lo - pad), hi + pad];
}

export function shortAge(hours: number): string {
  if (hours < 1) return `${Math.max(1, Math.round(hours * 60))} min`;
  if (hours < 48) return `${Math.round(hours)} h`;
  const days = hours / 24;
  if (days < 60) return `${Math.round(days)} d`;
  if (days < 730) return `${Math.round(days / 30)} mo`;
  return `${Math.round(days / 365)} y`;
}
