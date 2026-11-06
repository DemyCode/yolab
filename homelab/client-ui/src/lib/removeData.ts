export interface KeptFolder {
  folder: string;
  used_by: string[];
}

export interface DataVerdict {
  delete: string[];
  kept: KeptFolder[];
}

export function removePath(instance: string, deleteData: boolean): string {
  const query = deleteData ? "?delete_data=true" : "";
  return `/api/apps/${encodeURIComponent(instance)}${query}`;
}

export function keptReason(kept: KeptFolder): string {
  return `${kept.folder}, used by ${kept.used_by.join(", ")}`;
}
