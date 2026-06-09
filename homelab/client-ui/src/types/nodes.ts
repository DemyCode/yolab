export interface NodeInfo {
  name: string
  ip: string
  ready: boolean
  roles: string[]
  joined_at: string
}

export interface JoinInfo {
  k3s_token: string
  server_addr: string
}
