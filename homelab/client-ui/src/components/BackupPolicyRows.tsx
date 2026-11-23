import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Input, Select } from "@/components/ui/input";
import { Row } from "@/components/ui/list";
import { BACKUP_PRESETS, budgetToBytes, bytesToBudget } from "@/lib/backups";
import { isPreset } from "@/lib/settings";

export function ScheduleRow({
  schedule,
  detail,
  busy,
  onSave,
}: {
  schedule: string;
  detail?: ReactNode;
  busy: boolean;
  onSave: (schedule: string) => void;
}) {
  const [custom, setCustom] = useState(!isPreset(schedule));
  const [draft, setDraft] = useState(schedule);

  return (
    <Row
      label="How often"
      detail={detail}
      trailing={
        <Select
          value={custom ? "__custom" : schedule}
          onChange={(e) => {
            if (e.target.value === "__custom") {
              setDraft(schedule);
              setCustom(true);
              return;
            }
            setCustom(false);
            onSave(e.target.value);
          }}
          disabled={busy}
          aria-label="How often"
          className="h-9 w-auto max-w-[14rem]"
        >
          {BACKUP_PRESETS.map((p) => (
            <option key={p.cron} value={p.cron}>
              {p.label}
            </option>
          ))}
          <option value="__custom">Custom…</option>
        </Select>
      }
    >
      {custom && (
        <div className="mt-3">
          <div className="flex gap-2">
            <Input
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              aria-label="Custom schedule"
              spellCheck={false}
              className="h-9 font-mono"
            />
            <Button
              variant="secondary"
              size="sm"
              onClick={() => onSave(draft.trim())}
              loading={busy}
              disabled={!draft.trim() || draft.trim() === schedule}
            >
              Save
            </Button>
          </div>
          <p className="mt-1.5 text-xs text-fg-subtle">
            Minute, hour, day, month, weekday, in the server&rsquo;s time.{" "}
            <code className="font-mono">0 3 1 * *</code> is the 1st of each
            month at 03:00.
          </p>
        </div>
      )}
    </Row>
  );
}

export function SpaceRow({
  label,
  bytes,
  detail,
  busy,
  onSave,
  action,
}: {
  label: string;
  bytes: number;
  detail: ReactNode;
  busy: boolean;
  onSave: (bytes: number) => void;
  action?: ReactNode;
}) {
  const [draft, setDraft] = useState(bytesToBudget(bytes));
  const wanted = budgetToBytes(draft);

  return (
    <Row label={label} detail={detail}>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <div className="flex items-center gap-2">
          <Input
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            inputMode="decimal"
            aria-label="Space in GB"
            className="h-9 w-24 font-mono tabular-nums"
          />
          <span className="text-sm text-fg-muted">GB</span>
        </div>
        <Button
          variant="secondary"
          size="sm"
          onClick={() => wanted !== null && onSave(wanted)}
          loading={busy}
          disabled={wanted === null || wanted === bytes}
        >
          Save
        </Button>
        {action}
      </div>
    </Row>
  );
}
