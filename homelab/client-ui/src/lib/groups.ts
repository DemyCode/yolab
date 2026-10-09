import type { AppInfo } from "@/types/apps";

export interface Membership {
  name: string;
  title: string;
  main: boolean;
}

export interface HomeGroup {
  name: string;
  title: string;
  main: AppInfo[];
  others: AppInfo[];
}

export interface MemberStatus {
  state: "waiting" | "working" | "done" | "failed";
  message?: string;
}

export interface GroupRecord {
  name: string;
  title: string;
  chart: string;
  repo: string;
  version: string;
  values: Record<string, unknown>;
  members: Record<string, string>;
  status: Record<string, MemberStatus>;
  left: string[];
  reused: string[];
}

export interface GroupView extends GroupRecord {
  schema: object | null;
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

export interface MemberRow {
  key: string;
  instance: string;
  state: MemberStatus["state"];
  message: string;
}

export function memberRows(group: GroupRecord): MemberRow[] {
  return Object.entries(group.members)
    .map(([key, ns]) => ({
      key,
      instance: ns.replace(/^yolab-/, ""),
      state: group.status[key]?.state ?? "done",
      message: group.status[key]?.message ?? "",
    }))
    .sort((a, b) => a.key.localeCompare(b.key));
}

export function settingUp(group: GroupRecord): boolean {
  return Object.values(group.status).some(
    (s) => s.state === "waiting" || s.state === "working",
  );
}

export function groupNameFor(id: string, taken: string[]): string {
  if (!taken.includes(id)) return id;
  for (let n = 2; ; n++) {
    const candidate = `${id}-${n}`;
    if (!taken.includes(candidate)) return candidate;
  }
}

export function installedByGroup(group: GroupRecord): string[] {
  return memberRows(group)
    .filter((row) => !group.reused.includes(row.key))
    .map((row) => row.instance);
}
