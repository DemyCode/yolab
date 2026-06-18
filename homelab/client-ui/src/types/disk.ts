export type DiskStatus = "unused" | "joining" | "active" | "removing" | "missing";

export interface DiskItem {
  name: string;
  model: string;
  size_bytes: number;
  host: string;
  hostname: string;
  status: DiskStatus;
  is_builtin: boolean;
  osd_id: number | null;
  used_bytes: number | null;
  safe_to_destroy: boolean | null;
  /** True when removing this disk would leave zero active OSDs. */
  last_disk: boolean;
  /** Removing disks only: 0–100% of data migrated off this OSD. */
  migration_pct: number | null;
}

export interface DiskRequest {
  disk_name: string;
  host: string;
}

export interface CephStatus {
  available: boolean;
  health: string;
  osd_count: number;
  osd_up: number;
  total_bytes: number;
  used_bytes: number;
  error?: string;
}
