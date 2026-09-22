export interface StatusInfo {
  commit_hash: string;
  commit_message: string;
  commit_date: string;
  platform: string;
  flake_target: string;
  console_url?: string;
  error?: string;
}

export interface RebuildLog {
  running: boolean;
  log: string[];
}

export interface ChannelInfo {
  url: string;
  ref: string;
}
