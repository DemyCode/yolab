import type { CatalogApp } from "@/types/apps";

export interface AppStats {
  app_id: string;
  installs: number;
  rating_count: number;
  rating_average: number | null;
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
export const INSTALLS_SHOWN_FROM = 10;
export const RATING_SHOWN_FROM = 3;

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

export function shownInstalls(stats: AppStats | undefined): number | null {
  if (!stats || stats.installs < INSTALLS_SHOWN_FROM) return null;
  return stats.installs;
}

export function shownRating(
  stats: AppStats | undefined,
): { average: number; count: number } | null {
  if (!stats || stats.rating_average === null) return null;
  if (stats.rating_count < RATING_SHOWN_FROM) return null;
  return {
    average: Math.round(stats.rating_average * 10) / 10,
    count: stats.rating_count,
  };
}

function popularity(app: CatalogApp, stats: Map<string, AppStats>): number {
  const installs = shownInstalls(stats.get(app.id)) ?? 0;
  return installs * 1000 + (app.stars ?? 0);
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

export function factsSentence(
  app: Pick<CatalogApp, "stars" | "pushed_at">,
  stats: AppStats | undefined,
  now: Date = new Date(),
): string | null {
  const starred =
    app.stars && app.stars > 0
      ? `${app.stars.toLocaleString("en-US")} people starred it on GitHub`
      : null;
  const ago = app.pushed_at ? updatedAgo(app.pushed_at, now) : null;
  const updated = ago ? `updated ${ago}` : null;
  const upstream =
    starred && updated
      ? `${starred}, and it was ${updated}.`
      : starred
        ? `${starred}.`
        : updated
          ? `It was ${updated}.`
          : null;

  const installs = shownInstalls(stats);
  const rating = shownRating(stats);
  const yolab =
    installs !== null && rating
      ? `${installs.toLocaleString("en-US")} YoLab users run it, and they rate it ${rating.average} out of 5.`
      : installs !== null
        ? `${installs.toLocaleString("en-US")} YoLab users run it.`
        : rating
          ? `${rating.count} YoLab users rate it ${rating.average} out of 5.`
          : null;

  const said = [upstream, yolab].filter(Boolean).join(" ");
  return said || null;
}
