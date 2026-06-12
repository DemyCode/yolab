export interface DiskItem {
  name: string;
  model: string;
  size_bytes: number;
  host: string;
  hostname: string;
  is_osd: boolean;
  is_builtin: boolean;
  used_bytes: number | null;
  free_bytes: number | null;
}

export interface DiskOrderEntry {
  host: string;
  disk_name: string;
}

export interface DiskOrderRequest {
  entries: DiskOrderEntry[];
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
