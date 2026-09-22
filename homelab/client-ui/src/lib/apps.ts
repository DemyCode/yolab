import type { AppInfo, CatalogApp } from "@/types/apps";

export interface AppLink {
  label: string;
  url: string;
}

export function installedByChart(
  apps: AppInfo[] | undefined,
): Map<string, number> {
  const counts = new Map<string, number>();
  for (const a of apps ?? []) {
    counts.set(a.app_id, (counts.get(a.app_id) ?? 0) + 1);
  }
  return counts;
}

export function appLinks(app: AppInfo, tunnelDomain: string): AppLink[] {
  const links: AppLink[] = [];
  const seen = new Set<string>();

  for (const o of app.outputs ?? []) {
    if (o.type !== "url" || !o.value || seen.has(o.value)) continue;
    seen.add(o.value);
    links.push({ label: o.label || "Open", url: o.value });
  }

  const subdomain = app.config?.subdomain;
  if (typeof subdomain === "string" && subdomain && tunnelDomain) {
    const derived = `https://${subdomain}.${tunnelDomain}`;
    const already = [...seen].some(
      (u) => u.replace(/\/$/, "") === derived.replace(/\/$/, ""),
    );
    if (!already) links.push({ label: "Open", url: derived });
  }

  return links;
}

export interface AppFactRow {
  key: string;
  label: string;
  value: string | null;
}

export function appFactRows(app: AppInfo): AppFactRow[] {
  const found = new Map(
    (app.outputs ?? [])
      .filter((o) => o.type !== "url" && o.type !== "hidden" && o.value)
      .map((o) => [o.key, o]),
  );

  const rows: AppFactRow[] = [];
  const seen = new Set<string>();

  for (const spec of app.outputs_spec ?? []) {
    if (spec.type === "url" || spec.type === "hidden") continue;
    seen.add(spec.key);
    const hit = found.get(spec.key);
    rows.push({
      key: spec.key,
      label: hit?.label || spec.label || spec.key,
      value: hit?.value ?? null,
    });
  }

  for (const [key, o] of found) {
    if (!seen.has(key))
      rows.push({ key, label: o.label || key, value: o.value });
  }

  return rows;
}

export function nextInstanceName(appId: string, installed: AppInfo[]): string {
  const taken = new Set(installed.map((a) => a.instance_name));
  if (!taken.has(appId)) return appId;
  for (let n = 2; n < 100; n++) {
    const candidate = `${appId}-${n}`;
    if (!taken.has(candidate)) return candidate;
  }
  return `${appId}-${Date.now()}`;
}

export type AppState = "ready" | "starting" | "removing";

export function appState(app: AppInfo): AppState {
  if (app.status === "uninstalling") return "removing";
  if (app.status === "starting") return "starting";
  return "ready";
}

export function appLabel(app: AppInfo, state: AppState): string {
  return app.detail?.trim() || appStateLabel(state);
}

export function appStateLabel(state: AppState): string {
  switch (state) {
    case "starting":
      return "Starting up…";
    case "removing":
      return "Removing…";
    default:
      return "";
  }
}

export function catalogEntry(
  app: AppInfo,
  catalog: CatalogApp[],
): CatalogApp | undefined {
  return catalog.find((c) => c.id === app.app_id);
}

export function appDisplayName(app: AppInfo, catalog: CatalogApp[]): string {
  const entry = catalogEntry(app, catalog);
  const stem = instanceStem(app);
  if (!entry) return stem;
  return stem === app.app_id ? entry.name : stem;
}

export function instanceStem(app: AppInfo): string {
  const id = app.instance_id;
  return id && app.instance_name.endsWith(`-${id}`)
    ? app.instance_name.slice(0, -(id.length + 1))
    : app.instance_name;
}
