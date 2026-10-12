export type RestoreChoice = "use" | "replace" | "restore" | "empty";

export interface RestoreChoices {
  folders: Record<string, RestoreChoice>;
  data: Record<string, RestoreChoice>;
}

export interface FolderRow {
  name: string;
  title: string;
  capacity: string;
  backed_up: boolean;
  exists: boolean;
  used_by: string[];
  becomes: string | null;
}

export interface DataRow {
  claim: string;
  capacity: string;
  backed_up: boolean;
}

export interface RestorePlan {
  folders: FolderRow[];
  data: DataRow[];
}

export type RestoreMode = "reinstall" | "in-place";

export interface ChoiceOption {
  value: RestoreChoice;
  label: string;
}

export const NO_CHOICES: RestoreChoices = { folders: {}, data: {} };

export function folderOptions(row: FolderRow): ChoiceOption[] {
  if (row.exists) {
    return row.backed_up
      ? [
          { value: "use", label: "Use it as it is now" },
          { value: "replace", label: "Replace it with the backup" },
        ]
      : [{ value: "use", label: "Use it as it is now" }];
  }
  return row.backed_up
    ? [
        { value: "restore", label: "Bring it back from the backup" },
        { value: "empty", label: "Start it empty" },
      ]
    : [{ value: "empty", label: "Start it empty" }];
}

export function dataOptions(row: DataRow, mode: RestoreMode): ChoiceOption[] {
  if (mode === "in-place") {
    const keep: ChoiceOption = { value: "use", label: "Keep it as it is now" };
    if (!row.backed_up) return [keep];
    return [{ value: "replace", label: "Replace it with the backup" }, keep];
  }
  return row.backed_up
    ? [
        { value: "restore", label: "Bring it back from the backup" },
        { value: "empty", label: "Start it empty" },
      ]
    : [{ value: "empty", label: "Start it empty" }];
}

export function chosen(
  options: ChoiceOption[],
  picked: RestoreChoice | undefined,
): RestoreChoice {
  return options.find((o) => o.value === picked)?.value ?? options[0].value;
}

export function shownFolders(
  plan: RestorePlan,
  inSettings: string[] | null,
): FolderRow[] {
  if (inSettings === null) return plan.folders;
  return plan.folders.filter((f) => inSettings.includes(f.name));
}

export function choicesFor(
  plan: RestorePlan,
  mode: RestoreMode,
  picked: RestoreChoices,
  inSettings: string[] | null,
): RestoreChoices {
  const folders: Record<string, RestoreChoice> = {};
  for (const row of shownFolders(plan, inSettings)) {
    folders[row.name] = chosen(folderOptions(row), picked.folders[row.name]);
  }
  const data: Record<string, RestoreChoice> = {};
  for (const row of plan.data) {
    data[row.claim] = chosen(dataOptions(row, mode), picked.data[row.claim]);
  }
  return { folders, data };
}

export function sharedWarning(row: FolderRow, app: string): string | null {
  const others = row.used_by.filter((u) => u !== app);
  if (others.length === 0) return null;
  return `${others.join(", ")} ${others.length === 1 ? "uses" : "use"} it too, and would see the change.`;
}

export function settingValues(values: unknown): string[] {
  if (typeof values === "string") return values.trim() ? [values.trim()] : [];
  if (Array.isArray(values)) return values.flatMap(settingValues);
  if (values && typeof values === "object") {
    return Object.values(values).flatMap(settingValues);
  }
  return [];
}

export function usedHere(plan: RestorePlan, app: string): string[] {
  return plan.folders.filter((f) => f.used_by.includes(app)).map((f) => f.name);
}

export function renamedNote(row: FolderRow): string | null {
  if (!row.becomes) return null;
  return `Another folder is called ${row.name} now, so this one comes back as ${row.becomes}.`;
}
