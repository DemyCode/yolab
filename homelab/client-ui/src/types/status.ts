export interface StatusInfo {
  commit_hash: string;
  commit_message: string;
  commit_date: string;
  platform: string;
  flake_target: string;
  /** Account and billing console for this deployment, derived by the backend
   *  from platform_api_url. Absent when it cannot be worked out — the Settings
   *  row is then omitted rather than pointing somewhere that 404s. */
  console_url?: string;
  error?: string;
}

export interface RebuildLog {
  running: boolean;
  log: string[];
}

export interface RemoteEntry {
  name: string;
  url: string;
}

export interface ChannelInfo {
  remote: string;
  ref: string;
  remotes: RemoteEntry[];
}
