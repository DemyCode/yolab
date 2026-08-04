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
  status: "starting" | "running" | "uninstalling";
  outputs: AppOutput[];
  outputs_spec: OutputSpec[];
  config: Record<string, unknown>;
}

export interface CatalogApp {
  id: string;
  /// Repository the chart came from. "official" is the curated catalog; anything else
  /// was added by the user and can create arbitrary cluster objects, so the UI must be
  /// able to tell them apart rather than presenting all apps as equally vouched-for.
  repo: string;
  chart_version: string;
  name: string;
  description: string;
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

export interface DescribeResponse {
  output: string;
}

export interface DomainResponse {
  domain: string;
}
