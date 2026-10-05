import type { AppInfo, AppOutput, CatalogApp, PodInfo } from "@/types/apps";
import { roughDuration } from "./format";

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
    if (o.format !== "uri" || !o.value || seen.has(o.value)) continue;
    seen.add(o.value);
    links.push({ label: o.title || "Open", url: o.value });
  }

  const subdomain = app.config?.subdomain;
  const yolabOn = app.config?.yolab_enabled !== false;
  if (yolabOn && typeof subdomain === "string" && subdomain && tunnelDomain) {
    const derived = `https://${subdomain}.${tunnelDomain}`;
    const already = [...seen].some(
      (u) => u.replace(/\/$/, "") === derived.replace(/\/$/, ""),
    );
    if (!already) links.push({ label: "Open", url: derived });
  }

  return links;
}

export type OutputState = "ready" | "waiting" | "unset";

export function outputState(output: AppOutput): OutputState {
  if (output.value) return "ready";
  return output.from_config ? "unset" : "waiting";
}

export function accessRows(outputs: AppOutput[]): AppOutput[] {
  return outputs.filter((o) => !(o.format === "uri" && o.value));
}

export function stillWaiting(outputs: AppOutput[]): boolean {
  return outputs.some((o) => outputState(o) === "waiting");
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

export type AppState =
  "ready" | "starting" | "removing" | "copying" | "failed" | "stopped";

export function appState(app: AppInfo): AppState {
  if (app.status === "uninstalling") return "removing";
  if (app.status === "failed") return "failed";
  if (app.status === "stopped") return "stopped";
  if (app.status === "copying") return "copying";
  if (app.status === "starting") return "starting";
  return "ready";
}

export function appLabel(app: AppInfo, state: AppState): string {
  if (state === "failed" || state === "stopped") return appStateLabel(state);
  return app.detail?.trim() || appStateLabel(state);
}

export function appStateLabel(state: AppState): string {
  switch (state) {
    case "failed":
      return "Failed installation";
    case "stopped":
      return "Stopped working";
    case "copying":
      return "Copying files…";
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

export function newerVersion(
  app: AppInfo,
  entry: CatalogApp | undefined,
): string | null {
  if (!entry || !app.chart_version) return null;
  const listed = entry.chart_version;
  return listed === app.chart_version ? null : listed;
}

export function appAddress(app: AppInfo): string {
  const subdomain = app.config?.subdomain;
  return typeof subdomain === "string" ? subdomain : "";
}

export function appDisplayName(
  app: AppInfo,
  catalog: CatalogApp[],
  installed: AppInfo[] = [],
): string {
  const stem = instanceStem(app);
  if (stem !== app.app_id) return stem;
  const entry = catalogEntry(app, catalog);
  const base = entry?.name ?? stem;
  const copies = installed.filter((a) => a.app_id === app.app_id).length;
  if (copies < 2) return base;
  const address = appAddress(app);
  return address
    ? `${base} (${address})`
    : `${base} (${app.instance_id ?? ""})`;
}

export function instanceStem(app: AppInfo): string {
  const id = app.instance_id;
  return id && app.instance_name.endsWith(`-${id}`)
    ? app.instance_name.slice(0, -(id.length + 1))
    : app.instance_name;
}

export type AppAction =
  "update" | "retry" | "duplicate" | "backup" | "restore" | "remove";

export function availableActions(
  state: AppState,
  restoring: boolean,
): Set<AppAction> {
  if (state === "removing" || restoring) return new Set<AppAction>();
  switch (state) {
    case "failed":
      return new Set<AppAction>(["retry", "remove"]);
    case "starting":
    case "copying":
      return new Set<AppAction>(["duplicate", "restore", "remove"]);
    case "stopped":
      return new Set<AppAction>(["update", "duplicate", "restore", "remove"]);
    default:
      return new Set<AppAction>([
        "update",
        "duplicate",
        "backup",
        "restore",
        "remove",
      ]);
  }
}

export type StatusTone = "live" | "busy" | "warn" | "error";

export function appStatus(state: AppState): {
  tone: StatusTone;
  label: string;
} {
  switch (state) {
    case "failed":
      return { tone: "error", label: "Failed installation" };
    case "stopped":
      return { tone: "warn", label: "Stopped working" };
    case "removing":
      return { tone: "busy", label: "Being removed" };
    case "copying":
      return { tone: "busy", label: "Copying its files" };
    case "starting":
      return { tone: "busy", label: "Starting up" };
    default:
      return { tone: "live", label: "Running" };
  }
}

export interface RestoreRecord {
  id: string;
  namespace: string;
  snapshot_id?: string | null;
  started_at: string;
  finished_at?: string | null;
  error?: string | null;
  state: "running" | "succeeded" | "failed";
}

export const RESTORE_DONE_SHOWN_MS = 30 * 60 * 1000;

export function latestRestore(
  records: RestoreRecord[],
  namespace: string,
  now: number,
): RestoreRecord | null {
  const mine = records
    .filter((r) => r.namespace === namespace)
    .sort((a, b) => b.started_at.localeCompare(a.started_at));
  const latest = mine[0];
  if (!latest) return null;
  if (latest.state === "running") return latest;
  const ended = latest.finished_at ? Date.parse(latest.finished_at) : NaN;
  if (Number.isNaN(ended) || now - ended > RESTORE_DONE_SHOWN_MS) return null;
  return latest;
}

export const SLOW_START_MS = 10 * 60 * 1000;

export function waitNote(
  app: AppInfo,
  state: AppState,
  now: number,
): string | null {
  if (state === "failed" || state === "stopped") {
    if (!app.retry_at) return null;
    const left = Date.parse(app.retry_at) - now;
    if (Number.isNaN(left)) return null;
    if (left <= 5_000) return "Trying to start it again now…";
    return left < 45_000
      ? "Next try in a few seconds."
      : `Next try in about ${roughDuration(left)}.`;
  }
  if (state !== "starting" && state !== "copying" && state !== "removing") {
    return null;
  }
  if (!app.since) return null;
  const started = Date.parse(app.since);
  if (Number.isNaN(started)) return null;
  const waited = Math.max(0, now - started);
  if (state === "starting" && waited >= SLOW_START_MS) {
    return `It has been ${roughDuration(waited)}, longer than usual. Technical details below show what each part is doing.`;
  }
  return waited < 45_000
    ? "Started a few seconds ago."
    : `Started ${roughDuration(waited)} ago.`;
}

export function podStatus(pod: PodInfo): string {
  if (pod.ready) return "Running";
  switch (pod.phase) {
    case "Running":
      return "Starting";
    case "Pending":
      return "Getting ready";
    case "Succeeded":
      return "Finished its job";
    case "Failed":
      return "Stopped";
    default:
      return "Not responding";
  }
}
