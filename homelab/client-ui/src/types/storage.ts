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
