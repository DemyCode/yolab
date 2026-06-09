export type DiskState = "system" | "unformatted" | "waiting" | "active" | "ejecting"

export interface DiskInfo {
  name: string
  model: string
  size_bytes: number
  host: string
  hostname: string
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

export interface SystemOsdResizeResponse {
  ok: boolean
  operation: "extended" | "shrunk" | "unchanged"
}

export interface OkResponse {
  ok: boolean
}

export interface PriorityItem {
  host: string
  disk_name: string
  position: number
  model: string
  size_bytes: number
  state: DiskState | "offline"
  hostname: string
  used_bytes: number | null
  free_bytes: number | null
  can_eject: boolean
  ceph_osd_id: number | null
}

export interface PriorityUpdateRequest {
  entries: { host: string; disk_name: string }[]
}

export interface AddToStorageRequest {
  disk_name: string
  host: string
}

export interface EjectRequest {
  disk_name: string
  host: string
}
