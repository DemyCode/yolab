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
}

export interface RestorePoint {
  snapshot_id: string;
  time: string;
}

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

export function isBusy(state: ProtectionState): boolean {
  return state === "running" || state === "queued";
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

export function restorePointLabel(index: number): string {
  return index === 0 ? "Most recent" : "";
}
