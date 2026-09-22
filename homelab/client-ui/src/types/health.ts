export type HealthLevel = "ok" | "warn" | "error";

export interface HealthIssue {
  level: HealthLevel;
  title: string;
  description: string;
}

export interface ClusterHealth {
  level: HealthLevel;
  title: string;
  message: string;
  issues: HealthIssue[];
  starting: boolean;
  storage_unrecoverable?: boolean;
  provisioning: boolean;
}
