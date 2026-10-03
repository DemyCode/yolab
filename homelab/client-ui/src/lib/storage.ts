import { appDisplayName, catalogEntry } from "@/lib/apps";
import type { AppInfo, CatalogApp } from "@/types/apps";
import type {
  DiskInfo,
  Osd,
  Space,
  StorageOverview,
  StorageTarget,
  StorageUsage,
} from "@/types/storage";

export const POLL_IDLE_MS = 30_000;
export const POLL_MOVING_MS = 5_000;
export const POLL_USAGE_MS = 60_000;

export function formatCephBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log2(bytes) / 10));
  const value = bytes / 1024 ** i;
  const shown =
    i === 0 || value >= 100 ? Math.round(value) : Number(value.toFixed(1));
  return `${shown} ${units[i]}`;
}

export type Tone = "ok" | "warn" | "bad";

export function fillTone(pct: number): Tone {
  if (pct >= 85) return "bad";
  if (pct >= 70) return "warn";
  return "ok";
}

export function rawPercent(raw: StorageOverview["raw"]): number {
  if (raw.total_bytes <= 0) return 0;
  return Math.min(100, (raw.used_bytes / raw.total_bytes) * 100);
}

export type DiskState =
  | "active"
  | "pending"
  | "missing"
  | "draining"
  | "excluded"
  | "historical"
  | "foreign"
  | "unidentified"
  | "failing"
  | "blocked"
  | "stale"
  | "removing"
  | "removable";

export function diskState(disk: DiskInfo): DiskState {
  if (disk.ownership === "unknown") return "unidentified";
  if (disk.foreign_ceph) return "foreign";

  const on = disk.desired === "ON";
  if (on && !disk.connected) return "missing";
  if (!on && !disk.connected) return "historical";

  switch (disk.phase) {
    case "active":
      return "active";
    case "creating":
      return "pending";
    case "retrying":
      return "failing";
    case "blocked":
      return "blocked";
    case "draining":
      return "draining";
    case "removing":
      return "removing";
    case "removable":
      return "removable";
    case "unknown":
      return "stale";
  }

  if (on && disk.is_our_osd) return "active";
  if (on) return "pending";
  if (disk.is_our_osd) return "draining";
  return "excluded";
}

export type Domain = "osd" | "host";

export function placesFor(osds: Osd[], domain: Domain): number {
  const inUse = osds.filter((o) => o.weight > 0);
  if (domain === "osd") return inUse.length;
  return new Set(inUse.map((o) => o.host)).size;
}

export function statusLine(
  overview: StorageOverview | undefined,
  target: StorageTarget | null | undefined,
  happening: string | null = null,
): { tone: Tone; text: string } | null {
  if (!overview) return null;
  const disks = overview.osds.filter((o) => o.weight > 0).length;
  const machines = new Set(
    overview.osds.filter((o) => o.weight > 0).map((o) => o.host),
  ).size;
  const copies = overview.space?.copies ?? target?.size;
  const parts: string[] = [];
  if (copies)
    parts.push(`${copies} ${copies === 1 ? "copy" : "copies"} of everything`);
  parts.push(`${disks} ${disks === 1 ? "disk" : "disks"}`);
  if (machines > 1) parts.push(`${machines} machines`);
  const detail = parts.join(" · ");
  switch (overview.health) {
    case "HEALTH_OK":
      return { tone: "ok", text: `Healthy · ${detail}` };
    case "HEALTH_ERR":
      return { tone: "bad", text: `Needs you now · ${detail}` };
    default:
      return {
        tone: "warn",
        text: `${happening ?? "Needs attention"} · ${detail}`,
      };
  }
}

export interface BannerSpec {
  tone: "warning" | "error";
  title: string;
  body: string;
}

export function pickBanner({
  error,
  movementBlocked,
  overview,
  copies,
}: {
  error: string | null;
  movementBlocked: boolean;
  overview: StorageOverview | undefined;
  copies: number;
}): BannerSpec | "movement" | null {
  if (error) {
    return {
      tone: "error",
      title: "Storage is not answering",
      body: `Your apps may not be able to read or save files until it comes back. (${error})`,
    };
  }
  if (movementBlocked) return "movement";
  if (!overview) return null;

  const down = overview.osds.filter((o) => !o.up && o.host !== "");
  const phantom = overview.osds.filter((o) => !o.up && o.host === "");
  const anyUp = overview.osds.some((o) => o.up);

  if (down.length > 0 && (copies <= 1 || !anyUp)) {
    const those = down.length === 1 ? "that disk" : "those disks";
    return {
      tone: "error",
      title: "A disk is offline and there is no second copy",
      body: `Whatever was on ${those} is not readable right now, and it is not stored anywhere else — so do not switch it off. Get the disk back if you can; otherwise your backups are the only copy.`,
    };
  }
  if (down.length > 0) {
    return {
      tone: "warning",
      title: "A disk is offline",
      body: `Your files are still there — they are stored ${copies} times, so the other copies are serving them. If the disk does not come back, switch it off below and YoLab will rebuild the missing copies on the disks that remain.`,
    };
  }
  if (phantom.length > 0) {
    const it = phantom.length === 1 ? "it" : "them";
    return {
      tone: "warning",
      title: "A disk did not finish being set up",
      body: `${phantom.length === 1 ? "One disk" : `${phantom.length} disks`} started being added and never finished, so nothing is stored on ${it} yet. Switch ${it} off and on again to retry. Nothing is at risk.`,
    };
  }
  return null;
}

export function protectionLine(
  target: StorageTarget | null | undefined,
): { tone: Tone; text: string } | null {
  if (!target || target.size > 1) return null;
  const unit = target.failure_domain === "host" ? "machine" : "disk";
  return {
    tone: "bad",
    text: `Everything is stored once. If a ${unit} fails, what was on it is gone — backups are your only copy.`,
  };
}

export interface ChangeEstimate {
  freeAfter: number;
  extraNeeded: number;
  fit: "ok" | "tight" | "impossible";
}

export function estimateChange(
  space: Space,
  nextCopies: number,
  places: number,
): ChangeEstimate {
  const before = Math.max(1, Math.min(space.copies, Math.max(places, 1)));
  const after = Math.max(1, Math.min(nextCopies, Math.max(places, 1)));
  const stored = space.apps_bytes + space.images_bytes + space.other_bytes;
  const room = space.free_bytes * before;
  const extraNeeded = Math.max(0, (after - before) * stored);
  const freeAfter = Math.max(0, (room - extraNeeded) / after);
  const fit =
    extraNeeded === 0
      ? "ok"
      : room < extraNeeded
        ? "impossible"
        : room < extraNeeded * 1.3
          ? "tight"
          : "ok";
  return { freeAfter, extraNeeded, fit };
}

export interface UsageRow {
  key: string;
  label: string;
  bytes: number;
  instance?: string;
  appId?: string;
  icon?: string;
}

export function usageRows(
  usage: StorageUsage | undefined,
  apps: AppInfo[],
  catalog: CatalogApp[],
  imagesBytes: number,
): UsageRow[] {
  const rows: UsageRow[] = [];
  let other = 0;
  for (const u of usage?.apps ?? []) {
    const app = u.instance
      ? apps.find((a) => a.instance_name === u.instance)
      : undefined;
    if (!app) {
      other += u.bytes;
      continue;
    }
    rows.push({
      key: u.namespace,
      label: appDisplayName(app, catalog, apps),
      bytes: u.bytes,
      instance: app.instance_name,
      appId: app.app_id,
      icon: catalogEntry(app, catalog)?.icon,
    });
  }
  if (imagesBytes > 0) {
    rows.push({ key: "images", label: "App programs", bytes: imagesBytes });
  }
  if (other > 0) {
    rows.push({
      key: "other",
      label: "Removed or system volumes",
      bytes: other,
    });
  }
  return rows.sort((a, b) => b.bytes - a.bytes);
}
