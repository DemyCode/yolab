export type DiskState = "system" | "unformatted" | "waiting" | "active" | "ejecting"

export interface DiskInfo {
  name: string
  model: string
  size_bytes: number
  host: string
  state: DiskState
  ceph_osd_id: number | null
  used_bytes: number | null
  free_bytes: number | null
  can_eject: boolean
  queue_position: number | null
  fs_type: string | null
}

export interface EjectStatus {
  pg_count: number
  done: boolean
  safe_to_unplug: boolean
}

export interface CephStatus {
  available: boolean
  health: string
  osd_count: number
  osd_up: number
  total_bytes: number
  used_bytes: number
  error?: string
}

export interface SystemOsdInfo {
  exists: boolean
  size_bytes: number | null
  fs_free_bytes: number
  ceph_osd_id: number | null
}
