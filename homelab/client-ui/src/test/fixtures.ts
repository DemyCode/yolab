import type { AppInfo, CatalogApp } from "@/types/apps";

export function catalogApp(over: Partial<CatalogApp> = {}): CatalogApp {
  return {
    id: "gitea",
    repo: "official",
    chart_version: "1.0.0",
    name: "Gitea",
    description: "Git hosting",
    home: "",
    icon: "",
    category: "",
    github: "",
    tagline: "",
    collections: [],
    stars: null,
    pushed_at: null,
    schema: {},
    ...over,
  };
}

export function appInfo(over: Partial<AppInfo> = {}): AppInfo {
  return {
    app_id: "gitea",
    instance_name: "gitea",
    chart_version: "1.0.0",
    status: "running",
    detail: "",
    outputs: [],
    config: {},
    backup: { enabled: false, schedule: "", last_ok_at: null, running: false },
    ...over,
  };
}
