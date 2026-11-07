import { api, ApiError } from "./api";
import { revealedBy, revealedFields, type ConfigSchema } from "./schema";

export const TREE_FOLDER = "tree-folder";

export interface FolderListing {
  path: string;
  folders: string[];
  truncated: boolean;
}

export function cleanFolder(value: unknown): string {
  return String(value ?? "")
    .replace(/\/{2,}/g, "/")
    .replace(/^\/+|\/+$/g, "");
}

export function splitFolder(value: unknown): { parent: string; name: string } {
  const raw = String(value ?? "")
    .replace(/\/{2,}/g, "/")
    .replace(/^\/+/, "");
  const cut = raw.lastIndexOf("/");
  if (cut < 0) return { parent: "", name: raw };
  return { parent: raw.slice(0, cut), name: raw.slice(cut + 1) };
}

export function joinFolder(parent: string, name: string): string {
  return parent ? `${parent}/${name}` : name;
}

export const ROOT_FOLDER = "/";

export function isRootFolder(value: unknown): boolean {
  return String(value ?? "") !== "" && cleanFolder(value) === "";
}

export function shownFolder(path: string): string {
  return `/${cleanFolder(path)}`;
}

export function folderTrail(path: string): { name: string; path: string }[] {
  const parts = cleanFolder(path).split("/").filter(Boolean);
  return [
    { name: ROOT_FOLDER, path: "" },
    ...parts.map((name, i) => ({
      name,
      path: parts.slice(0, i + 1).join("/"),
    })),
  ];
}

function subsequence(needle: string, hay: string): boolean {
  let i = 0;
  for (const c of hay) {
    if (c === needle[i]) i += 1;
    if (i === needle.length) return true;
  }
  return needle.length === 0;
}

export function rankFolders(
  folders: string[],
  typed: string,
  limit = 8,
): string[] {
  const t = typed.toLowerCase();
  const score = (f: string): number => {
    const l = f.toLowerCase();
    if (l === t) return 0;
    if (l.startsWith(t)) return 1;
    if (l.includes(t)) return 2;
    if (subsequence(t, l)) return 3;
    return -1;
  };
  return folders
    .map((f) => [f, score(f)] as const)
    .filter(([, s]) => s >= 0)
    .sort(
      (a, b) =>
        a[1] - b[1] || a[0].length - b[0].length || a[0].localeCompare(b[0]),
    )
    .slice(0, limit)
    .map(([f]) => f);
}

export function folderIn(listing: string[], name: string): boolean {
  return name !== "" && listing.includes(name);
}

const KEEP_MS = 15_000;
const cache = new Map<
  string,
  { at: number; listing: Promise<FolderListing> }
>();

export function listFolders(parent: string): Promise<FolderListing> {
  const path = cleanFolder(parent);
  const hit = cache.get(path);
  if (hit && Date.now() - hit.at < KEEP_MS) return hit.listing;
  const listing = api.get<FolderListing>(
    `/api/tree/folders?path=${encodeURIComponent(path)}`,
  );
  cache.set(path, { at: Date.now(), listing });
  listing.catch(() => cache.delete(path));
  return listing;
}

export function activeTreeFolders(
  schema: ConfigSchema,
  values: Record<string, unknown>,
): { name: string; title: string; value: string }[] {
  const toggleOf = new Map<string, string>();
  for (const [toggle, names] of revealedBy(schema)) {
    for (const n of names) toggleOf.set(n, toggle);
  }
  const fields = [
    ...Object.entries(schema.properties ?? {}),
    ...revealedFields(schema),
  ].filter(([, p]) => p.format === TREE_FOLDER);
  return fields
    .filter(([name]) => {
      const toggle = toggleOf.get(name);
      return toggle === undefined || values[toggle] === true;
    })
    .map(([name, p]) => ({
      name,
      title: p.title ?? name,
      value: isRootFolder(values[name])
        ? ROOT_FOLDER
        : cleanFolder(values[name]),
    }));
}

export async function folderProblem(
  fields: { title: string; value: string }[],
  list: (parent: string) => Promise<FolderListing> = listFolders,
): Promise<string | null> {
  for (const f of fields) {
    if (!f.value) return `Pick the ${f.title.toLowerCase()}.`;
    if (isRootFolder(f.value)) continue;
    const { parent, name } = splitFolder(f.value);
    let listing: FolderListing;
    try {
      listing = await list(parent);
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) {
        return `There is no folder ${f.value} in your file tree.`;
      }
      const why = e instanceof Error ? e.message : String(e);
      return `The file tree could not be read: ${why}`;
    }
    if (!folderIn(listing.folders, name)) {
      return `There is no folder ${f.value} in your file tree.`;
    }
  }
  return null;
}
