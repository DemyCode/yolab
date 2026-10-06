export type OutputFormat = "text" | "uri" | "secret" | "multiline";

export interface AppOutput {
  key: string;
  title: string;
  format: OutputFormat;
  value: string | null;
  found_at: string | null;
  from_config: boolean;
}

export interface AppInfo {
  app_id: string;
  instance_name: string;
  instance_id?: string | null;
  chart_version: string;
  status:
    "starting" | "running" | "uninstalling" | "copying" | "failed" | "stopped";
  detail: string;
  technical?: string;
  since?: string | null;
  retry_at?: string | null;
  outputs: AppOutput[];
  config: Record<string, unknown>;
  backup: AppBackupStatus;
}

export interface AppBackupStatus {
  enabled: boolean;
  schedule: string;
  last_ok_at: string | null;
  running: boolean;
}

export interface AppDefinition {
  schema: number;
  app_id: string;
  chart_repo: string;
  chart_version: string;
  instance_name: string;
  service_name: string;
  config: Record<string, unknown>;
  volumes: { name: string; capacity: string }[];
  resources: {
    cpu_millicores: number;
    memory_bytes: number;
    gpu: number;
    replicas: number;
  };
  backup: AppBackupStatus;
}

export interface CatalogApp {
  id: string;
  repo: string;
  chart_version: string;
  name: string;
  description: string;
  home: string;
  icon: string;
  category: string;
  github: string;
  tagline: string;
  collections: string[];
  stars: number | null;
  pushed_at: string | null;
  schema: object;
}

export interface PodInfo {
  name: string;
  phase: string;
  ready: boolean;
}

export interface OutputsResponse {
  outputs: AppOutput[];
}

export interface DomainResponse {
  domain: string;
}
