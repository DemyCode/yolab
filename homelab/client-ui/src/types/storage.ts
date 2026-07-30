export interface OsdInfo {
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
  status: string;
  crush_weight: number;
  reweight: number;
  safe_to_destroy: boolean;
  ok_to_stop: boolean;
}

export interface PoolInfo {
  id: number;
  name: string;
  size: number;
  min_size: number;
  crush_rule_name: string;
  failure_domain: string;
  stored_bytes: number;
  used_bytes: number;
  max_avail_bytes: number;
}

export interface StorageDetail {
  osds: OsdInfo[];
  pools: PoolInfo[];
  total_bytes: number;
  avail_bytes: number;
  used_bytes: number;
}

export interface StorageDetailResponse {
  ok: boolean;
  data?: StorageDetail;
  error?: string;
}

export interface DiskInfo {
  id: string;
  device: string;
  model: string;
  size_bytes: number;
  is_loop: boolean;
  is_our_osd: boolean;
  foreign_ceph: boolean;
  osd_id: number | null;
  /** "ON" = user wants in cluster, "OFF" = excluded. Legacy "USING" treated as ON. */
  desired: "ON" | "OFF" | "USING";
  connected: boolean;
}

export interface StoragePolicy {
  mode: "auto" | "manual";
  size: number;
  min_size: number;
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
  policy: StoragePolicy;
  topology: StorageTopology;
  target: StorageTarget;
}
