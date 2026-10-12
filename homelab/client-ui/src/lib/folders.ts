import { formatBytes } from "@/lib/format";

export interface Folder {
  name: string;
  title: string;
  size: string;
  ready: boolean;
  used_by: string[];
  kind: "files" | "state";
  app: string | null;
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
