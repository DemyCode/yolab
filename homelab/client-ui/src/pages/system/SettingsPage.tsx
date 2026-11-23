import { useState } from "react";
import { Section } from "@/components/ui/list";
import { Banner, Spinner } from "@/components/ui/feedback";
import { ScheduleRow, SpaceRow } from "@/components/BackupPolicyRows";
import { SshKeysCard } from "@/components/SshKeysCard";
import { useApi } from "@/lib/useResource";
import {
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
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);

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
      <ScheduleRow
        schedule={settings.backup_schedule}
        detail="Each service you install from now on starts with this. Services already installed keep their own, which you change on their backup page."
        busy={busy}
        onSave={(schedule) =>
          void save({ ...settings, backup_schedule: schedule })
        }
      />
      <SpaceRow
        label="Space for each service's backups"
        bytes={settings.backup_budget_bytes}
        detail="When a service's copies need more than this, some are deleted until the rest fit, keeping them spread back through time. The newest copy always stays."
        busy={busy}
        onSave={(bytes) =>
          void save({ ...settings, backup_budget_bytes: bytes })
        }
      />
    </Section>
  );
}

export function SettingsPage() {
  const settings = useApi<ClusterSettings>("cluster-settings", SETTINGS_PATH);

  return (
    <>
      {settings.data ? (
        <BackupDefaults
          key={`${settings.data.backup_schedule}-${settings.data.backup_budget_bytes}`}
          settings={settings.data}
          onSaved={settings.refresh}
        />
      ) : settings.error ? (
        <Banner tone="error" title="The backup settings could not be read">
          {settings.error}
        </Banner>
      ) : (
        <div className="flex justify-center py-12">
          <Spinner />
        </div>
      )}
      <div className="mt-8">
        <SshKeysCard />
      </div>
    </>
  );
}
