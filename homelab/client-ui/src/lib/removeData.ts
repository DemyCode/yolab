export interface FolderUse {
  folder: string;
  used_by: string[];
}

export interface DataVerdict {
  delete: string[];
  shared: FolderUse[];
  kept: FolderUse[];
}

export function removePath(instance: string, deleteShared: boolean): string {
  const query = deleteShared ? "?delete_shared=true" : "";
  return `/api/apps/${encodeURIComponent(instance)}${query}`;
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
