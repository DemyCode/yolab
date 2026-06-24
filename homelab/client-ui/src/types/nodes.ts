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

