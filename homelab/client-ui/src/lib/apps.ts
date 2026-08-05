import type { AppInfo, CatalogApp } from "@/types/apps";

/**
 * Where an installed app actually lives.
 *
 * Two sources, in order of trust: an output the chart scraped from the pod's
 * own logs (which is the app telling us its URL), then the subdomain the user
 * chose combined with the box's tunnel domain. The second is a reconstruction,
 * but it is right for every chart in the catalog and it is available
 * immediately — outputs only appear after the pod has started and been
 * scanned, and "Open" must work before then or the tile is dead on arrival.
 */
export function appUrl(app: AppInfo, tunnelDomain: string): string | null {
  const output = app.outputs?.find((o) => o.type === "url" && o.value);
  if (output) return output.value;

  const subdomain = app.config?.subdomain;
  if (typeof subdomain === "string" && subdomain && tunnelDomain) {
    return `https://${subdomain}.${tunnelDomain}`;
  }
  return null;
}

export interface AppLink {
  label: string;
  url: string;
}

/**
 * Every web address an app exposes, not just the first.
 *
 * An app is not one link. A chart can scrape several `url` outputs — an admin
 * panel beside the app itself, a second front end, an API endpoint — and once
 * bundles of apps land, one install will routinely publish half a dozen. So
 * this returns a list and the caller renders all of them.
 *
 * The reconstructed `subdomain.domain` address is appended only when nothing
 * already covers it, so it acts as the answer before the first output scan has
 * run rather than as a duplicate afterwards.
 */
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
    // Charts write the URL with and without a trailing slash; compare loosely
    // so we do not offer the same address twice under two labels.
    const already = [...seen].some(
      (u) => u.replace(/\/$/, "") === derived.replace(/\/$/, ""),
    );
    if (!already) links.push({ label: "Open", url: derived });
  }

  return links;
}

/**
 * Values that are not links: a server address to paste into Minecraft, an
 * IPv6 for a game client, a generated admin password. These are the whole
 * point of the app page for anything that is not a website.
 */
export function appFacts(app: AppInfo) {
  return (app.outputs ?? []).filter(
    (o) => o.type !== "url" && o.type !== "hidden" && o.value,
  );
}

/**
 * A free name for another copy of the same app.
 *
 * Installing an app twice is a normal thing to want — a family photo library
 * and a private one, a work Vaultwarden and a personal one, a test blog beside
 * the real one. The backend has always supported it: each install gets its own
 * namespace (`yolab-<instance>`) and the chart id is kept in an annotation, so
 * `nextcloud` and `nextcloud-2` are two independent apps. Only the UI assumed
 * one install per chart.
 *
 * The name doubles as the namespace and the default subdomain, so it has to be
 * a valid DNS label and it has to be unused.
 */
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

/** What the tile says under the name. Empty for the healthy case. */
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

/** Match an installed instance back to its catalog entry, for icons and copy. */
export function catalogEntry(
  app: AppInfo,
  catalog: CatalogApp[],
): CatalogApp | undefined {
  return catalog.find((c) => c.id === app.app_id);
}

/**
 * A display name for an installed instance.
 *
 * `instance_name` is what Helm calls the release, which for a second copy of an
 * app is something like "immich-2". Prefer the catalog's display name and only
 * fall back to the release name when the chart is gone from the catalog.
 */
export function appDisplayName(app: AppInfo, catalog: CatalogApp[]): string {
  const entry = catalogEntry(app, catalog);
  if (!entry) return app.instance_name;
  // A renamed instance is meaningful information — show it rather than
  // pretending every copy is "Immich".
  return app.instance_name === app.app_id ? entry.name : app.instance_name;
}
