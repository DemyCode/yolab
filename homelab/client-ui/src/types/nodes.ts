export interface NodeInfo {
  name: string;
  ip: string;
  ready: boolean;
  roles: string[];
  joined_at: string;
}

export interface NodeLink {
  name: string;
  url: string;
}

export interface NodeTransferEntry {
  node_id: number;
  sub_ipv6: string;
  rx_bytes: number;
  tx_bytes: number;
  last_seen: string | null;
}

export interface TrafficPoint {
  ts: string;
  rx: number;
  tx: number;
}

export interface MonthlyTraffic {
  year_month: string;
  bytes: number;
}

export interface TrafficData {
  nodes: NodeTransferEntry[];
  current_month: MonthlyTraffic;
  hourly: TrafficPoint[];
  daily: TrafficPoint[];
  monthly: MonthlyTraffic[];
}
