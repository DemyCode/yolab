import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input, Select } from "@/components/ui/input";
import { Row, RowAction, Section } from "@/components/ui/list";
import { Banner, Spinner } from "@/components/ui/feedback";
import { BACKUP_PRESETS, budgetToBytes, bytesToBudget } from "@/lib/backups";
import { useApi } from "@/lib/useResource";
import {
  isPreset,
  saveSettings,
  SETTINGS_PATH,
  type ClusterSettings,
} from "@/lib/settings";
import { cn } from "@/lib/utils";

function BackupDefaults({
  settings,
  onSaved,
}: {
  settings: ClusterSettings;
  onSaved: () => Promise<void>;
}) {
  const [custom, setCustom] = useState(!isPreset(settings.backup_schedule));
  const [schedule, setSchedule] = useState(settings.backup_schedule);
  const [space, setSpace] = useState(
    settings.backup_budget_bytes
      ? bytesToBudget(settings.backup_budget_bytes)
      : "",
  );
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const wantedSpace = budgetToBytes(space);

  async function save(next: ClusterSettings) {
    setBusy(true);
    setNote(null);
    const saved = await saveSettings(next);
    setBusy(false);
    if (!saved.ok) {
      setNote({ ok: false, text: saved.error });
      return;
    }
    setNote({ ok: true, text: "Saved" });
    await onSaved();
  }

  return (
    <Section
      title="Backups of new services"
      action={
        note && (
          <span
            className={cn("text-xs", note.ok ? "text-success" : "text-danger")}
            role="status"
          >
            {note.text}
          </span>
        )
      }
    >
      <Row
        label="How often"
        detail="Each service you install from now on starts with this. Services already installed keep their own, which you change on their page."
        trailing={
          <Select
            value={custom ? "__custom" : settings.backup_schedule}
            onChange={(e) => {
              if (e.target.value === "__custom") {
                setSchedule(settings.backup_schedule);
                setCustom(true);
                return;
              }
              setCustom(false);
              void save({ ...settings, backup_schedule: e.target.value });
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
                value={schedule}
                onChange={(e) => setSchedule(e.target.value)}
                aria-label="Custom schedule"
                spellCheck={false}
                className="h-9 font-mono"
              />
              <Button
                variant="secondary"
                size="sm"
                onClick={() =>
                  void save({ ...settings, backup_schedule: schedule.trim() })
                }
                loading={busy}
                disabled={
                  !schedule.trim() ||
                  schedule.trim() === settings.backup_schedule
                }
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
      <Row
        label="Space for each service's backups"
        detail={
          settings.backup_budget_bytes
            ? "When a service's copies need more than this, as many as fit are kept. The newest copy always stays."
            : "Chosen for each service: twice the size of its newest copy. Set a size to give every new service the same space."
        }
      >
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <div className="flex items-center gap-2">
            <Input
              value={space}
              onChange={(e) => setSpace(e.target.value)}
              inputMode="decimal"
              aria-label="Space in GB"
              className="h-9 w-24 font-mono tabular-nums"
            />
            <span className="text-sm text-fg-muted">GB</span>
          </div>
          <Button
            variant="secondary"
            size="sm"
            onClick={() =>
              void save({ ...settings, backup_budget_bytes: wantedSpace })
            }
            loading={busy}
            disabled={
              wantedSpace === null ||
              wantedSpace === settings.backup_budget_bytes
            }
          >
            Save
          </Button>
          {settings.backup_budget_bytes !== null && (
            <RowAction
              onClick={() => {
                setSpace("");
                void save({ ...settings, backup_budget_bytes: null });
              }}
              disabled={busy}
            >
              Let YoLab choose
            </RowAction>
          )}
        </div>
      </Row>
    </Section>
  );
}

export function SettingsPage() {
  const settings = useApi<ClusterSettings>("cluster-settings", SETTINGS_PATH);

  if (!settings.data) {
    return settings.error ? (
      <Banner tone="error" title="The settings could not be read">
        {settings.error}
      </Banner>
    ) : (
      <div className="flex justify-center py-12">
        <Spinner />
      </div>
    );
  }
  return (
    <BackupDefaults
      key={`${settings.data.backup_schedule}-${settings.data.backup_budget_bytes ?? "auto"}`}
      settings={settings.data}
      onSaved={settings.refresh}
    />
  );
}
