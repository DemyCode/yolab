export interface StatusInfo {
  commit_hash: string
  commit_message: string
  commit_date: string
  platform: string
  flake_target: string
  error?: string
}

export interface RebuildLog {
  running: boolean
  log: string[]
}

export interface RemoteEntry {
  name: string
  url: string
}

export interface ChannelInfo {
  remote: string
  ref: string
  remotes: RemoteEntry[]
}
