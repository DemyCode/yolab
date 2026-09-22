export interface PathStatus {
  node: string;
  path: "direct" | "relayed";
  endpoint: string | null;
  handshake_age_secs: number | null;
}
