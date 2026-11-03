import { Spinner } from "@/components/ui/feedback";
import { Select } from "@/components/ui/input";
import { Row } from "@/components/ui/list";
import {
  chosen,
  dataOptions,
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

export function RestoreData({
  plan,
  error,
  mode,
  app,
  picked,
  onPick,
}: {
  plan: RestorePlan | null;
  error: string | null;
  mode: RestoreMode;
  app: string;
  picked: RestoreChoices;
  onPick: (next: RestoreChoices) => void;
}) {
  if (error) {
    return <Row label="Its data" detail={error} />;
  }
  if (!plan) {
    return (
      <Row
        label="Its data"
        detail={
          <span className="inline-flex items-center gap-2">
            <Spinner className="h-4 w-4" />
            Reading the backup…
          </span>
        }
      />
    );
  }
  return (
    <>
      {plan.data.map((row) => {
        const options = dataOptions(row, mode);
        return (
          <Row
            key={row.claim}
            label={`${app}'s own data`}
            detail={row.backed_up ? undefined : "Not in this backup"}
            trailing={
              <ChoiceSelect
                options={options}
                value={chosen(options, picked.data[row.claim])}
                label={`${app}'s own data`}
                onChange={(v) =>
                  onPick({ data: { ...picked.data, [row.claim]: v } })
                }
              />
            }
          />
        );
      })}
    </>
  );
}
