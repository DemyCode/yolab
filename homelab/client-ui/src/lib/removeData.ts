export interface FolderUse {
  folder: string;
  used_by: string[];
}

export interface DataVerdict {
  delete: string[];
  shared: FolderUse[];
  kept: FolderUse[];
}

export function removePath(
  instance: string,
  deleteShared: boolean,
  deleteBackups = false,
): string {
  const params = new URLSearchParams();
  if (deleteShared) params.set("delete_shared", "true");
  if (deleteBackups) params.set("delete_backups", "true");
  const query = params.toString();
  return `/api/apps/${encodeURIComponent(instance)}${query ? `?${query}` : ""}`;
}

export function folderName(folder: string): string {
  return folder === "" ? "/ (the whole file tree)" : `/${folder}`;
}

export function usedBy(use: FolderUse): string {
  return `${folderName(use.folder)}, used by ${use.used_by.join(", ")}`;
}

export function sharedWith(shared: FolderUse[]): string[] {
  return [...new Set(shared.flatMap((s) => s.used_by))].sort();
}
