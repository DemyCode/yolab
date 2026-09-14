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
    running: boolean;
    started_at: number;
    finished_at: number | null;
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
  restored: string[];
  not_restored: string[];
}

export const RECOVERY_STEPS: { step: RecoveryStep; label: string }[] = [
  { step: "purge_osds", label: "Forgetting the lost disks" },
  { step: "remove_apps", label: "Removing apps" },
  { step: "delete_storage", label: "Deleting the damaged storage" },
  { step: "recreate_storage", label: "Creating fresh storage" },
  { step: "restart_csi", label: "Restarting the storage driver" },
  { step: "reinstall_apps", label: "Reinstalling apps from backup" },
];

/** A finished recovery stays on the page this long, so its outcome can be read. */
const SHOW_FINISHED_FOR_SECS = 7 * 24 * 3600;

export function useRecoveryStatus() {
  return useApi<RecoveryStatus>("storage-recovery", "/api/storage/recovery", {
    pollMs: 5_000,
  });
}

/** Whether there is anything about lost data the owner has to act on or watch. */
export function recoveryNeedsAttention(s: RecoveryStatus | undefined): boolean {
  return Boolean(s?.loss?.needs_recovery || s?.recovery?.running);
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
