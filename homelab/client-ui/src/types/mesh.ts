/** Mirrors local-api's mesh::PathStatus. */
export interface PathStatus {
  /** The peer's cluster address — matches NodeInfo.ip, which is how the two lists join. */
  node: string;
  path: "direct" | "relayed";
  endpoint: string | null;
  handshake_age_secs: number | null;
}
