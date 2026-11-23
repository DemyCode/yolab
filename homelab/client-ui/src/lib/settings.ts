import { api, ApiError } from "./api";
import { BACKUP_PRESETS } from "./backups";

export interface ClusterSettings {
  backup_schedule: string;
  backup_budget_bytes: number | null;
}

export const SETTINGS_PATH = "/api/settings";

export type Saved =
  { ok: true; settings: ClusterSettings } | { ok: false; error: string };

export async function saveSettings(next: ClusterSettings): Promise<Saved> {
  try {
    return {
      ok: true,
      settings: await api.put<ClusterSettings>(SETTINGS_PATH, next),
    };
  } catch (e) {
    return {
      ok: false,
      error: e instanceof ApiError ? e.message : "The settings were not saved.",
    };
  }
}

export function isPreset(schedule: string): boolean {
  return BACKUP_PRESETS.some((p) => p.cron === schedule);
}
