import { useApi } from "@/lib/useResource";

/** `GET /api/storage/recovery` — see storage_heal.rs `status_json`. */
export interface RecoveryStatus {
  loss: {
    osds: number[];
    pools: string[];
    placement_groups: number;
    needs_recovery: boolean;
    detected_at: number;
  } | null;
  recovery: {
    step: RecoveryStep;
    steps: RecoveryStep[];
    running: boolean;
    started_at: number;
    finished_at: number | null;
    /** Of the whole run. 100 only once it has finished. */
    percent: number;
    /** How far the current step has got, when it can be counted. */
    step_progress: { done: number; total: number; detail?: string } | null;
    apps: {
      namespace: string;
      instance_name: string;
      outcome:
        | { result: "restored" }
        | { result: "failed"; error: string }
        | null;
    }[];
    /** Null until the backup has been read. */
    not_restored: string[] | null;
  } | null;
}

export type RecoveryStep =
  | "purge_osds"
  | "remove_apps"
  | "delete_storage"
  | "recreate_storage"
  | "restart_csi"
  | "reinstall_apps";

/** `GET /api/storage/recovery/preview` */
export interface RecoveryPreview {
  backup_taken_at: string | null;
  /** OSD ids that are down right now. */
  down_osds: number[];
  restored: string[];
  not_restored: string[];
}

export const RECOVERY_STEP_LABELS: Record<RecoveryStep, string> = {
  purge_osds: "Forgetting the lost disks",
  remove_apps: "Removing apps",
  delete_storage: "Deleting the damaged storage",
  recreate_storage: "Creating fresh storage",
  restart_csi: "Restarting the storage driver",
  reinstall_apps: "Reinstalling apps from backup",
};

/** A finished recovery stays on the page this long, so its outcome can be read. */
const SHOW_FINISHED_FOR_SECS = 7 * 24 * 3600;

export function useRecoveryStatus() {
  return useApi<RecoveryStatus>("storage-recovery", "/api/storage/recovery", {
    pollMs: 3_000,
  });
}

/** Where each step stands relative to the one the recovery is on. */
export function stepState(
  steps: RecoveryStep[],
  current: RecoveryStep,
  step: RecoveryStep,
  running: boolean,
): "done" | "current" | "pending" {
  if (!running) return "done";
  const at = steps.indexOf(current);
  const i = steps.indexOf(step);
  if (i < at) return "done";
  return i === at ? "current" : "pending";
}

const DISMISSED_KEY = "yolab-recovery-dismissed";

/** A finished recovery's summary is shown until the owner has read it. */
export function recoveryDismissed(startedAt: number): boolean {
  try {
    return localStorage.getItem(DISMISSED_KEY) === String(startedAt);
  } catch {
    return false;
  }
}

export function dismissRecovery(startedAt: number) {
  try {
    localStorage.setItem(DISMISSED_KEY, String(startedAt));
  } catch {
    /* storage unavailable: the summary shows again next time, which is harmless */
  }
}

export function finishedRecently(
  r: RecoveryStatus["recovery"],
  nowSecs: number,
): boolean {
  return (
    r !== null &&
    !r.running &&
    r.finished_at !== null &&
    nowSecs - r.finished_at < SHOW_FINISHED_FOR_SECS
  );
}
