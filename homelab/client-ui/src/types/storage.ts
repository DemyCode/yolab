export interface Osd {
  id: number;
  name: string;
  host: string;
  class: string;
  size_bytes: number;
  used_bytes: number;
  avail_bytes: number;
  utilization: number;
  var: number;
  pgs: number;
  up: boolean;
  weight: number;
  reweight: number;
}

export interface Pool {
  id: number;
  name: string;
  copies: number;
  min_copies: number;
  stored_bytes: number;
  used_bytes: number;
  max_avail_bytes: number;
}

export interface Space {
  free_bytes: number;
  apps_bytes: number;
  images_bytes: number;
  other_bytes: number;
  copies: number;
  fullest_disk_percent: number;
}

export interface RawUsage {
  total_bytes: number;
  used_bytes: number;
  avail_bytes: number;
  data_bytes: number;
}

export interface StorageOverview {
  health: string;
  space: Space | null;
  raw: RawUsage;
  osds: Osd[];
  pools: Pool[];
}

export interface AppUsage {
  namespace: string;
  instance: string | null;
  bytes: number;
}

export interface StorageUsage {
  apps: AppUsage[];
  unreadable: number;
}

export interface OsdChecks {
  ok_to_stop: number[];
  safe_to_destroy: number[];
}

export interface DiskInfo {
  id: string;
  device: string;
  model: string;
  size_bytes: number;
  is_loop: boolean;
  is_our_osd: boolean;
  foreign_ceph: boolean;
  ownership?: "ours" | "foreign" | "blank" | "unknown";
  osd_id: number | null;
  desired: "ON" | "OFF";
  connected: boolean;
  has_partitions: boolean;
  mounted: boolean;
  phase: DiskPhase | "";
  message: string;
  attempts: number;
}

export type DiskPhase =
  | "active"
  | "creating"
  | "retrying"
  | "blocked"
  | "draining"
  | "removing"
  | "removable"
  | "unknown";

export interface StoragePolicy {
  size: number;
  failure_domain: "osd" | "host";
}

export interface StorageTopology {
  nodes: number;
  osds: number;
}

export interface StorageTarget {
  size: number;
  min_size: number;
  failure_domain: string;
  mon: number;
  mgr: number;
}

export interface StoragePolicyData {
  policy: StoragePolicy | null;
  topology: StorageTopology | null;
  target: StorageTarget | null;
}

export type MovementState =
  "settled" | "working" | "unavailable" | "no_room" | "restarting" | "unknown";

export type MovementJobKind =
  "move" | "add_copies" | "rebuild" | "clone" | "free_space" | "repair";

export type MoveReason = "draining" | "filling" | "resizing" | "balancing";

export interface MovementJob {
  kind: MovementJobKind;
  unit: "bytes" | "percent";
  remaining_bytes: number;
  to_move_bytes: number;
  moved_bytes: number;
  progress: number | null;
  eta_secs: number | null;
}

export interface Movement {
  state: MovementState;
  jobs: MovementJob[];
  move_reason: MoveReason | null;
  draining: { node: string; name: string }[];
  filling: { node: string; name: string }[];
  waiting_for: string[];
  copies: number | null;
  clones: number;
  repairing: boolean;
  eta_secs: number | null;
  inactive_pgs: number;
  total_pgs: number;
}
