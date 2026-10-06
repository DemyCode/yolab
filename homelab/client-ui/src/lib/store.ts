import type { CatalogApp } from "@/types/apps";
import { groupFor } from "@/catalog/meta";

export interface AppStats {
  app_id: string;
  installs: number;
  hearts: number;
  comment_count: number;
}

export interface StoreComment {
  id: number;
  author: string;
  body: string;
  created_at: string;
  installed: boolean;
  mine: boolean;
}

export interface Collection {
  id: string;
  title: string;
  blurb: string;
}

export const COLLECTIONS: Collection[] = [
  {
    id: "start-here",
    title: "Start here",
    blurb: "What most people set up first",
  },
  {
    id: "replace-google",
    title: "Leave Google",
    blurb: "Photos, files, documents and search, kept at home",
  },
  {
    id: "family",
    title: "For the family",
    blurb: "Recipes, groceries, budgets and the baby",
  },
  {
    id: "watch-and-listen",
    title: "Watch and listen",
    blurb: "Films, music, books and podcasts you own",
  },
  {
    id: "privacy",
    title: "Privacy first",
    blurb: "Passwords, codes and sharing that never leave your hands",
  },
  {
    id: "for-developers",
    title: "For developers",
    blurb: "Code, automation and monitoring",
  },
  {
    id: "play",
    title: "Game night",
    blurb: "Servers for you and your friends",
  },
];

export const COLLECTION_SIZE = 6;

export type SortOrder = "popular" | "name" | "updated";

export function formatCount(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "0";
  if (n < 1000) return String(Math.round(n));
  const [value, unit] = n < 1_000_000 ? [n / 1000, "k"] : [n / 1_000_000, "M"];
  const shown = value >= 100 ? Math.round(value) : Number(value.toFixed(1));
  return `${shown}${unit}`;
}

export function statsById(
  stats: AppStats[] | undefined,
): Map<string, AppStats> {
  return new Map((stats ?? []).map((s) => [s.app_id, s]));
}



function popularity(app: CatalogApp, stats: Map<string, AppStats>): number {
  const s = stats.get(app.id);
  return ((s?.installs ?? 0) + (s?.hearts ?? 0)) * 1000 + (app.stars ?? 0);
}

export function sortApps(
  apps: CatalogApp[],
  order: SortOrder,
  stats: Map<string, AppStats>,
): CatalogApp[] {
  const byName = (a: CatalogApp, b: CatalogApp) => a.name.localeCompare(b.name);
  const sorted = [...apps];
  if (order === "name") return sorted.sort(byName);
  if (order === "updated") {
    const when = (a: CatalogApp) =>
      a.pushed_at ? new Date(a.pushed_at).getTime() || 0 : 0;
    return sorted.sort((a, b) => when(b) - when(a) || byName(a, b));
  }
  return sorted.sort(
    (a, b) => popularity(b, stats) - popularity(a, stats) || byName(a, b),
  );
}

export function inCollection(
  apps: CatalogApp[],
  collection: string,
  stats: Map<string, AppStats>,
): CatalogApp[] {
  return sortApps(
    apps.filter((a) => (a.collections ?? []).includes(collection)),
    "popular",
    stats,
  ).slice(0, COLLECTION_SIZE);
}

export function githubUrl(app: Pick<CatalogApp, "github">): string | null {
  return /^[\w.-]+\/[\w.-]+$/.test(app.github ?? "")
    ? `https://github.com/${app.github}`
    : null;
}

export function hasCommunity(app: Pick<CatalogApp, "repo">): boolean {
  return app.repo === "official";
}

const DAY_MS = 24 * 60 * 60 * 1000;

export function updatedAgo(iso: string, now: Date = new Date()): string | null {
  const days = Math.floor((now.getTime() - new Date(iso).getTime()) / DAY_MS);
  if (!Number.isFinite(days) || days < 0) return null;
  if (days === 0) return "today";
  if (days === 1) return "yesterday";
  if (days < 30) return `${days} days ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return months === 1 ? "a month ago" : `${months} months ago`;
  return "over a year ago";
}

export const SIMILAR_SIZE = 6;

export function similarApps(
  app: CatalogApp,
  apps: CatalogApp[],
  stats: Map<string, AppStats>,
): CatalogApp[] {
  const group = groupFor(app);
  const mine = new Set(app.collections ?? []);
  const closeness = (other: CatalogApp) =>
    (groupFor(other) === group ? 3 : 0) +
    (other.collections ?? []).filter((c) => mine.has(c)).length;
  const scored = apps
    .filter((other) => other.id !== app.id)
    .map((other) => ({ other, score: closeness(other) }))
    .filter(({ score }) => score > 0);
  return scored
    .sort(
      (a, b) =>
        b.score - a.score ||
        popularity(b.other, stats) - popularity(a.other, stats) ||
        a.other.name.localeCompare(b.other.name),
    )
    .slice(0, SIMILAR_SIZE)
    .map(({ other }) => other);
}
