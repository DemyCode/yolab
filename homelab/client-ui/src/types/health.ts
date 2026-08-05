export type HealthLevel = "ok" | "warn" | "error";

export interface HealthIssue {
  level: HealthLevel;
  title: string;
  description: string;
}

/** `GET /api/cluster/health` — already carries human sentences from local-api's
 *  `translate_health_check`, which is why the UI can render it verbatim. */
export interface ClusterHealth {
  level: HealthLevel;
  title: string;
  message: string;
  issues: HealthIssue[];
  /** Storage is coming up after a boot — expected, not a problem. */
  starting: boolean;
  /** A newly added disk is being prepared — expected, not a problem. */
  provisioning: boolean;
}
