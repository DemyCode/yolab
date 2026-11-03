export type RestoreChoice = "use" | "replace" | "restore" | "empty";

export interface RestoreChoices {
  data: Record<string, RestoreChoice>;
}

export interface DataRow {
  claim: string;
  capacity: string;
  backed_up: boolean;
}

export interface RestorePlan {
  data: DataRow[];
}

export type RestoreMode = "reinstall" | "in-place";

export interface ChoiceOption {
  value: RestoreChoice;
  label: string;
}

export const NO_CHOICES: RestoreChoices = { data: {} };

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

export function choicesFor(
  plan: RestorePlan,
  mode: RestoreMode,
  picked: RestoreChoices,
): RestoreChoices {
  const data: Record<string, RestoreChoice> = {};
  for (const row of plan.data) {
    data[row.claim] = chosen(dataOptions(row, mode), picked.data[row.claim]);
  }
  return { data };
}
