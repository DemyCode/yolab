export type DiskStatus = "pending" | "joining" | "active" | "draining" | "missing";

export interface DiskItem {
  name: string;
  model: string;
  size_bytes: number;
  host: string;
  hostname: string;
  status: DiskStatus;
  is_builtin: boolean;
  used_bytes: number | null;
  free_bytes: number | null;
  osd_id: number | null;
  safe_to_destroy: boolean | null;
}

export interface DrainRequest {
  disk_name: string;
  host: string;
  force?: boolean;
}

export interface RemoveRequest {
  disk_name: string;
  host: string;
  force?: boolean;
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
