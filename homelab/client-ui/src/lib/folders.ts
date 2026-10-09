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

export function usedByLine(folder: Folder): string {
  const apps = folder.used_by;
  if (apps.length === 0) return "No app uses it yet";
  if (apps.length <= 3) return `Used by ${apps.join(", ")}`;
  return `Used by ${apps.slice(0, 2).join(", ")} and ${apps.length - 2} more`;
}

export const SIZE_CHOICES_GIB = [100, 500, 1024, 2048, 4096];
export const DEFAULT_SIZE_GIB = 1024;
