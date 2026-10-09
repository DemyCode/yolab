import type { Folder } from "@/lib/folders";
import type { AppInfo } from "@/types/apps";

export interface Membership {
  name: string;
  title: string;
  main: boolean;
}

export interface SetupFolder {
  title: string;
}

export interface SetupApp {
  chart: string;
  settings?: Record<string, unknown>;
  folders?: Record<string, string>;
  uses?: Record<string, string>;
}

export interface Setup {
  title: string;
  tagline?: string;
  main?: string;
  folders?: Record<string, SetupFolder>;
  apps: Record<string, SetupApp>;
}

export interface CatalogSetup extends Setup {
  id: string;
}

export interface HomeGroup {
  name: string;
  title: string;
  main: AppInfo[];
  others: AppInfo[];
}

export function arrangeHome(apps: AppInfo[]): {
  loose: AppInfo[];
  groups: HomeGroup[];
} {
  const loose: AppInfo[] = [];
  const byName = new Map<string, HomeGroup>();
  for (const app of apps) {
    const g = app.group;
    if (!g) {
      loose.push(app);
      continue;
    }
    const group = byName.get(g.name) ?? {
      name: g.name,
      title: g.title,
      main: [],
      others: [],
    };
    (g.main ? group.main : group.others).push(app);
    byName.set(g.name, group);
  }
  const groups = [...byName.values()].map((g) => {
    if (g.main.length > 0) return g;
    const [first, ...rest] = g.others;
    return { ...g, main: [first], others: rest };
  });
  groups.sort((a, b) => a.title.localeCompare(b.title));
  return { loose, groups };
}

export type FolderPlan =
  { kind: "existing"; name: string } | { kind: "new"; title: string };

export type AppPlan =
  { kind: "new" } | { kind: "existing"; instance: string } | { kind: "skip" };

export interface SetupPlan {
  title: string;
  folders: Record<string, FolderPlan>;
  apps: Record<string, AppPlan>;
}

export function sameApp(installed: AppInfo[], chart: string): AppInfo[] {
  return installed.filter(
    (a) => a.app_id === chart && a.status !== "uninstalling",
  );
}

export function defaultPlan(
  setup: Setup,
  installed: AppInfo[],
  folders: Folder[],
): SetupPlan {
  const folderPlans: Record<string, FolderPlan> = {};
  for (const [key, f] of Object.entries(setup.folders ?? {})) {
    const match = folders.find(
      (existing) =>
        existing.name === key ||
        existing.title.toLowerCase() === f.title.toLowerCase(),
    );
    folderPlans[key] = match
      ? { kind: "existing", name: match.name }
      : { kind: "new", title: f.title };
  }
  const appPlans: Record<string, AppPlan> = {};
  for (const [key, app] of Object.entries(setup.apps)) {
    const found = sameApp(installed, app.chart);
    appPlans[key] =
      found.length === 1
        ? { kind: "existing", instance: found[0].instance_name }
        : { kind: "new" };
  }
  return { title: setup.title, folders: folderPlans, apps: appPlans };
}

export function setupConfig(
  seed: Record<string, unknown>,
  app: SetupApp,
  folderNames: Record<string, string>,
  instances: Record<string, string> = {},
): Record<string, unknown> {
  const config: Record<string, unknown> = { ...seed, ...(app.settings ?? {}) };
  for (const [field, key] of Object.entries(app.folders ?? {})) {
    const name = folderNames[key];
    if (name) config[field] = name;
  }
  for (const [field, key] of Object.entries(app.uses ?? {})) {
    const instance = instances[key];
    if (instance) config[field] = { from: `yolab-${instance}` };
  }
  return config;
}

export function usedFolders(setup: Setup, plan: SetupPlan): Set<string> {
  const used = new Set<string>();
  for (const [key, app] of Object.entries(setup.apps)) {
    if (plan.apps[key]?.kind !== "new") continue;
    for (const folder of Object.values(app.folders ?? {})) used.add(folder);
  }
  return used;
}

export function installOrder(setup: Setup): string[] {
  const keys = Object.keys(setup.apps);
  const first = setup.main && keys.includes(setup.main) ? [setup.main] : [];
  const order: string[] = [];
  const visit = (key: string, path: Set<string>) => {
    if (order.includes(key) || path.has(key) || !setup.apps[key]) return;
    path.add(key);
    for (const used of Object.values(setup.apps[key].uses ?? {})) {
      visit(used, path);
    }
    path.delete(key);
    order.push(key);
  };
  for (const key of [...first, ...keys]) visit(key, new Set());
  return order;
}
