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
