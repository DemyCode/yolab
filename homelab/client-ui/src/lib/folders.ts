import { formatBytes } from "@/lib/format";

export interface Folder {
  name: string;
  title: string;
  size: string;
  ready: boolean;
  used_by: string[];
}

export type FolderChoice =
  | { kind: "inside" }
  | { kind: "folder"; folder: Folder }
  | { kind: "missing"; name: string };

export function folderChoice(value: string, folders: Folder[]): FolderChoice {
  if (value === "") return { kind: "inside" };
  const folder = folders.find((f) => f.name === value);
  return folder ? { kind: "folder", folder } : { kind: "missing", name: value };
}

const UNITS: Record<string, number> = {
  Ki: 2 ** 10,
  Mi: 2 ** 20,
  Gi: 2 ** 30,
  Ti: 2 ** 40,
  Pi: 2 ** 50,
};

export function folderSize(size: string): string {
  const m = /^(\d+)(Ki|Mi|Gi|Ti|Pi)$/.exec(size.trim());
  if (!m) return size;
  return formatBytes(Number(m[1]) * UNITS[m[2]], 0);
}

export interface OnRemoval {
  deleted: string[];
  kept: { title: string; used_by: string[] }[];
}

function joined(names: string[]): string {
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

export function removalNote(removal: OnRemoval | null): string {
  if (!removal) {
    return "The spaces only it uses are deleted with their files about an hour after it is removed; a space another app uses is kept for it.";
  }
  const parts: string[] = [];
  if (removal.deleted.length > 0) {
    const one = removal.deleted.length === 1;
    parts.push(
      `${joined(removal.deleted)} ${one ? "is" : "are"} used by no other app, so ${one ? "it is" : "they are"} deleted with ${one ? "its" : "their"} files about an hour after it is removed.`,
    );
  }
  for (const kept of removal.kept) {
    parts.push(`${kept.title} stays for ${joined(kept.used_by)}.`);
  }
  return parts.join(" ");
}
