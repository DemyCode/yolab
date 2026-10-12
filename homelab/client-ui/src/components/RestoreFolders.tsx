import { Spinner } from "@/components/ui/feedback";
import { Select } from "@/components/ui/input";
import { Row } from "@/components/ui/list";
import { folderSize } from "@/lib/folders";
import {
  chosen,
  dataOptions,
  folderOptions,
  sharedWarning,
  shownFolders,
  type ChoiceOption,
  type RestoreChoice,
  type RestoreChoices,
  type RestoreMode,
  type RestorePlan,
} from "@/lib/restore";

function ChoiceSelect({
  options,
  value,
  label,
  onChange,
}: {
  options: ChoiceOption[];
  value: RestoreChoice;
  label: string;
  onChange: (v: RestoreChoice) => void;
}) {
  if (options.length === 1) {
    return <span className="text-sm text-fg-muted">{options[0].label}</span>;
  }
  return (
    <Select
      value={value}
      onChange={(e) => onChange(e.target.value as RestoreChoice)}
      aria-label={label}
      className="h-9 w-auto max-w-[15rem]"
    >
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </Select>
  );
}

export function RestoreFolders({
  plan,
  error,
  mode,
  app,
  picked,
  onPick,
  inSettings,
}: {
  plan: RestorePlan | null;
  error: string | null;
  mode: RestoreMode;
  app: string;
  picked: RestoreChoices;
  onPick: (next: RestoreChoices) => void;
  inSettings: string[] | null;
}) {
  if (error) {
    return <Row label="Its files" detail={error} />;
  }
  if (!plan) {
    return (
      <Row
        label="Its files"
        detail={
          <span className="inline-flex items-center gap-2">
            <Spinner className="h-4 w-4" />
            Reading the backup…
          </span>
        }
      />
    );
  }

  const folders = shownFolders(plan, inSettings);
  return (
    <>
      {plan.data.map((row) => {
        const options = dataOptions(row, mode);
        return (
          <Row
            key={`data-${row.claim}`}
            label={`${app}'s own data`}
            detail={
              row.backed_up
                ? row.kept && mode === "reinstall"
                  ? "Kept from when it was removed"
                  : undefined
                : "Not in this backup"
            }
            trailing={
              <ChoiceSelect
                options={options}
                value={chosen(options, picked.data[row.claim])}
                label={`${app}'s own data`}
                onChange={(v) =>
                  onPick({
                    ...picked,
                    data: { ...picked.data, [row.claim]: v },
                  })
                }
              />
            }
          />
        );
      })}
      {folders.map((row) => {
        const options = folderOptions(row);
        const value = chosen(options, picked.folders[row.name]);
        const shared = value === "replace" ? sharedWarning(row, app) : null;
        const detail = [
          row.exists ? "Still here" : "No longer here",
          row.backed_up ? null : "not in this backup",
          row.capacity ? `room for ${folderSize(row.capacity)}` : null,
        ]
          .filter(Boolean)
          .join(" · ");
        return (
          <Row
            key={`folder-${row.name}`}
            label={row.title}
            detail={
              <>
                {detail}
                {!row.exists && mode === "reinstall" && (
                  <span className="block text-xs text-fg-subtle">
                    Or pick another folder in the settings above.
                  </span>
                )}
                {shared && (
                  <span className="block text-xs text-warning">{shared}</span>
                )}
              </>
            }
            trailing={
              <ChoiceSelect
                options={options}
                value={value}
                label={row.title}
                onChange={(v) =>
                  onPick({
                    ...picked,
                    folders: { ...picked.folders, [row.name]: v },
                  })
                }
              />
            }
          />
        );
      })}
    </>
  );
}
