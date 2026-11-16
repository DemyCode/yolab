import { formatBytes } from "@/lib/format";

export type ProtectionState =
  "ok" | "running" | "queued" | "failed" | "never" | "off";

export interface ProtectedApp {
  namespace: string;
  instance_name: string;
  app_id: string;
  enabled: boolean;
  schedule: string;
  state: "ok" | "running" | "queued" | "failed" | "never";
  last_ok_at: string | null;
  error: string | null;
  warning?: string | null;
  size_bytes?: number | null;
  budget_bytes?: number | null;
  budget_set?: boolean;
  over_budget?: boolean;
}

export interface RestorePoint {
  snapshot_id: string;
  time: string;
  keep?: boolean;
  older?: boolean;
  newest?: boolean;
  added_bytes?: number | null;
}

export interface RestorePoints {
  configured: boolean;
  points: RestorePoint[];
  size_bytes?: number | null;
  budget_bytes?: number | null;
  budget_set?: boolean;
  over_budget?: boolean;
}

export interface BackedUpService {
  namespace: string;
  instance_name: string;
  installed: boolean;
  versions: RestorePoint[];
  size_bytes?: number | null;
}

export const BACKUP_PRESETS: { label: string; cron: string }[] = [
  { label: "Every day at 03:00", cron: "0 3 * * *" },
  { label: "Every 6 hours", cron: "0 */6 * * *" },
  { label: "Every Sunday at 03:00", cron: "0 3 * * 0" },
  { label: "On the 1st of each month", cron: "0 3 1 * *" },
];

export function protectionState(app: ProtectedApp): ProtectionState {
  if (!app.enabled) return "off";
  return app.state;
}

export function protectionLabel(state: ProtectionState): string {
  switch (state) {
    case "ok":
      return "Saved";
    case "running":
      return "Saving now";
    case "queued":
      return "Waiting its turn";
    case "failed":
      return "Last save failed";
    case "never":
      return "Not saved yet";
    case "off":
      return "Backups off";
  }
}

export function protectionTone(
  state: ProtectionState,
): "success" | "info" | "danger" | "muted" {
  switch (state) {
    case "ok":
      return "success";
    case "running":
    case "queued":
      return "info";
    case "failed":
      return "danger";
    default:
      return "muted";
  }
}

export function hoursSince(iso: string, now: number): number {
  return (now - new Date(iso).getTime()) / 3_600_000;
}

export function isStale(
  app: ProtectedApp,
  now: number,
  staleAfterHours: number,
): boolean {
  if (!app.enabled) return false;
  if (app.state === "failed") return true;
  if (!app.last_ok_at) return app.state === "never";
  return hoursSince(app.last_ok_at, now) >= staleAfterHours;
}

export function needsAttention(
  apps: ProtectedApp[],
  now: number,
  staleAfterHours: number,
): ProtectedApp[] {
  return apps.filter((a) => isStale(a, now, staleAfterHours));
}

export function restorePath(
  appId: string,
  namespace: string,
  snapshotId: string,
): string {
  return `/add/${appId}?restore=${encodeURIComponent(namespace)}&snapshot=${encodeURIComponent(snapshotId)}`;
}

export function sortRestorePoints(points: RestorePoint[]): RestorePoint[] {
  return points
    .slice()
    .sort((a, b) => new Date(b.time).getTime() - new Date(a.time).getTime());
}

const GB = 1_000_000_000;

export function usageLabel(
  size: number | null | undefined,
  budget: number | null | undefined,
): string | null {
  if (size == null) return null;
  if (budget == null) return formatBytes(size);
  return `${formatBytes(size)} of ${formatBytes(budget)}`;
}

export function budgetToBytes(gigabytes: string): number | null {
  const n = Number(gigabytes.trim().replace(",", "."));
  if (!Number.isFinite(n) || n <= 0) return null;
  return Math.round(n * GB);
}

export function bytesToBudget(bytes: number): string {
  return String(Math.round((bytes / GB) * 10) / 10);
}

export function pointsPath(namespace: string): string {
  return `/api/backups/apps/${encodeURIComponent(namespace)}/points`;
}

export function pointPath(namespace: string, snapshotId: string): string {
  return `${pointsPath(namespace)}/${encodeURIComponent(snapshotId)}`;
}

export function removedServices(all: BackedUpService[]): BackedUpService[] {
  return all
    .filter((s) => !s.installed && s.versions.length > 0)
    .sort((a, b) => a.namespace.localeCompare(b.namespace));
}

export function serviceName(namespace: string): string {
  return namespace.replace(/^yolab-/, "");
}

export function canDelete(point: RestorePoint): boolean {
  return !point.keep;
}
