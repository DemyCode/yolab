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

export function serviceName(namespace: string): string {
  return namespace.replace(/^yolab-/, "");
}

export function canDelete(point: RestorePoint): boolean {
  return !point.keep;
}

export interface BackupService {
  namespace: string;
  name: string;
  appId: string | null;
  installed: boolean;
  protection: ProtectedApp | null;
  copies: number;
  sizeBytes: number | null;
}

export function backupServices(
  running: ProtectedApp[],
  backedUp: BackedUpService[],
): BackupService[] {
  const byNamespace = new Map<string, BackupService>();
  for (const s of backedUp) {
    if (!s.installed && s.versions.length === 0) continue;
    byNamespace.set(s.namespace, {
      namespace: s.namespace,
      name: s.instance_name || serviceName(s.namespace),
      appId: null,
      installed: s.installed,
      protection: null,
      copies: s.versions.length,
      sizeBytes: s.size_bytes ?? null,
    });
  }
  for (const app of running) {
    const known = byNamespace.get(app.namespace);
    byNamespace.set(app.namespace, {
      namespace: app.namespace,
      name: app.instance_name,
      appId: app.app_id,
      installed: true,
      protection: app,
      copies: known?.copies ?? 0,
      sizeBytes: app.size_bytes ?? known?.sizeBytes ?? null,
    });
  }
  return [...byNamespace.values()].sort((a, b) => a.name.localeCompare(b.name));
}

export function copiesLabel(count: number): string {
  return count === 1 ? "1 copy" : `${count} copies`;
}

export function backupPagePath(namespace: string): string {
  return `/system/backups/${encodeURIComponent(namespace)}`;
}

const HOUR_MS = 3_600_000;

export function timelinePlaces(
  points: RestorePoint[],
  now: number,
): Map<string, number> {
  const ages = points.map((p) => Math.max(0, now - new Date(p.time).getTime()));
  const oldest = Math.max(HOUR_MS, ...ages);
  const scale = Math.log1p(oldest / HOUR_MS);
  return new Map(
    points.map((p, i) => [
      p.snapshot_id,
      1 - Math.log1p(ages[i] / HOUR_MS) / scale,
    ]),
  );
}

export function ageLabel(iso: string, now: number): string {
  const hours = Math.max(0, (now - new Date(iso).getTime()) / HOUR_MS);
  if (hours < 1) return "now";
  if (hours < 24) return `${Math.round(hours)} h ago`;
  const days = hours / 24;
  if (days < 60) return `${Math.round(days)} d ago`;
  if (days < 730) return `${Math.round(days / 30)} mo ago`;
  return `${Math.round(days / 365)} y ago`;
}
