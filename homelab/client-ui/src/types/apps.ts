export interface AppOutput {
  key: string;
  label: string;
  value: string;
  type: "url" | "text" | "hidden";
}

export interface OutputSpec {
  key: string;
  label: string;
  type: "url" | "text" | "hidden";
}

export interface AppInfo {
  app_id: string;
  instance_name: string;
  instance_id?: string | null;
  status: "starting" | "running" | "uninstalling";
  detail: string;
  outputs: AppOutput[];
  outputs_spec: OutputSpec[];
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
  schema: object;
  uischema: object;
}

export interface PodInfo {
  name: string;
  phase: string;
  ready: boolean;
}

export interface ScanOutputsResponse {
  outputs: AppOutput[];
}

export interface DomainResponse {
  domain: string;
}
