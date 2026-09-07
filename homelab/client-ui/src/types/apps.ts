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
  /** Plain-language explanation of `status`, empty when healthy. Written by the
   *  backend (routers/apps.rs `explain_app_state`) rather than derived here: the
   *  distinction between "downloading" and "crash looping" only exists in the pod
   *  status, which the UI never sees. */
  detail: string;
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
  /// The project's own website, from the chart's `home` field. Empty when the chart
  /// does not declare one, in which case no link is shown.
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

export interface DescribeResponse {
  output: string;
}

export interface DomainResponse {
  domain: string;
}
