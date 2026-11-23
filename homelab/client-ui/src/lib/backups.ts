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

export const STALE_AFTER_HOURS = 36;

const HOUR_MS = 3_600_000;

export function ageLabel(iso: string, now: number): string {
  const hours = Math.max(0, (now - new Date(iso).getTime()) / HOUR_MS);
  if (hours < 1) return "now";
  if (hours < 24) return `${Math.round(hours)} h ago`;
  const days = hours / 24;
  if (days < 60) return `${Math.round(days)} d ago`;
  if (days < 730) return `${Math.round(days / 30)} mo ago`;
  return `${Math.round(days / 365)} y ago`;
}

export type ServiceTone = "ok" | "busy" | "warn" | "error";

export function serviceTone(
  service: BackupService,
  now: number,
): ServiceTone {
  const app = service.protection;
  if (!app) return "ok";
  const state = protectionState(app);
  if (state === "failed") return "error";
  if (state === "running" || state === "queued") return "busy";
  if (isStale(app, now, STALE_AFTER_HOURS) || app.over_budget) return "warn";
  return "ok";
}

export function scheduleLabel(cron: string): string {
  return (
    BACKUP_PRESETS.find((p) => p.cron === cron)?.label ?? `Custom: ${cron}`
  );
}

export function serviceDetail(service: BackupService, now: number): string {
  const app = service.protection;
  if (!app) return `Removed · ${copiesLabel(service.copies)} left`;
  const state = protectionState(app);
  if (state === "off") {
    return service.copies > 0
      ? `Backups off · ${copiesLabel(service.copies)} kept`
      : "Backups off";
  }
  if (state !== "ok") return protectionLabel(state);
  const saved = app.last_ok_at
    ? `Saved ${ageLabel(app.last_ok_at, now)}`
    : "Saved";
  const over = app.over_budget ? " · More than its space" : "";
  return `${saved} · ${scheduleLabel(app.schedule)}${over}`;
}

export function byStorage(services: BackupService[]): BackupService[] {
  return services
    .slice()
    .sort(
      (a, b) =>
        (b.sizeBytes ?? -1) - (a.sizeBytes ?? -1) ||
        a.name.localeCompare(b.name),
    );
}

export function storageTotal(services: BackupService[]): number | null {
  const known = services.filter((s) => s.sizeBytes != null);
  if (known.length === 0) return null;
  return known.reduce((sum, s) => sum + (s.sizeBytes ?? 0), 0);
}

export function spaceToFit(sizeBytes: number): number {
  return Math.max(1, Math.ceil(sizeBytes / GB)) * GB;
}

export type ServiceBanner =
  | { kind: "failed"; error: string }
  | { kind: "over"; sizeBytes: number; budgetBytes: number }
  | { kind: "stale" }
  | null;

export function serviceBanner(
  app: ProtectedApp | undefined,
  copies: RestorePoints | null,
  now: number,
): ServiceBanner {
  if (!app) return null;
  if (app.enabled && app.state === "failed") {
    return { kind: "failed", error: app.error ?? "The last backup failed." };
  }
  if (
    copies?.over_budget &&
    copies.size_bytes != null &&
    copies.budget_bytes != null
  ) {
    return {
      kind: "over",
      sizeBytes: copies.size_bytes,
      budgetBytes: copies.budget_bytes,
    };
  }
  if (isStale(app, now, STALE_AFTER_HOURS)) return { kind: "stale" };
  return null;
}

export interface PruneResult {
  deleted: number;
  size_bytes: number;
  budget_bytes: number;
  over_budget: boolean;
}

export function prunePath(namespace: string): string {
  return `/api/backups/apps/${encodeURIComponent(namespace)}/prune`;
}

export function pruneLabel(result: PruneResult): string {
  const used = `${formatBytes(result.size_bytes)} of ${formatBytes(result.budget_bytes)} used`;
  if (result.over_budget) {
    return `Nothing deleted: its newest copy alone needs more than ${formatBytes(result.budget_bytes)}.`;
  }
  if (result.deleted === 0) return `Already within its space · ${used}`;
  return `Deleted ${copiesLabel(result.deleted)} · ${used}`;
}
