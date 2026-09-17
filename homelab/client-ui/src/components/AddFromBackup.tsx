import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { History } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/input";
import { Sheet } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import { formatDateTime } from "@/lib/format";
import { useApi } from "@/lib/useResource";

/** `GET /api/backups/apps` — see backups.rs `backed_up_apps_json`. */
interface BackedUpApps {
  configured: boolean;
  apps: {
    namespace: string;
    instance_name: string;
    installed: boolean;
    /** Newest first. */
    versions: { snapshot_id: string; time: string }[];
  }[];
}

function AppRow({ app }: { app: BackedUpApps["apps"][number] }) {
  const navigate = useNavigate();
  const [snapshot, setSnapshot] = useState(app.versions[0]?.snapshot_id ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  /**
   * Restoring opens the install form, prefilled from the backup, rather than
   * running a background job. The person gets to see — and change — the name and
   * web address the restored app will take before anything is created, which is
   * also where a subdomain collision is caught.
   */
  async function restore() {
    setBusy(true);
    setError(null);
    try {
      const def = await api.get<{ app_id: string }>(
        `/api/backups/apps/${app.namespace}/definition?snapshot_id=${encodeURIComponent(snapshot)}`,
      );
      navigate(
        `/add/${def.app_id}?restore=${encodeURIComponent(app.namespace)}&snapshot=${encodeURIComponent(snapshot)}`,
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not read that backup");
    } finally {
      setBusy(false);
    }
  }

  return (
    <li className="space-y-2 border-b border-border py-3 last:border-0">
      <div className="flex items-center justify-between gap-3">
        <span className="font-medium text-fg">{app.instance_name}</span>
        {app.installed ? (
          <span className="text-sm text-fg-muted">Installed</span>
        ) : null}
      </div>
      {!app.installed && (
        <div className="flex flex-col gap-2 sm:flex-row">
          <Select
            value={snapshot}
            onChange={(e) => setSnapshot(e.target.value)}
            aria-label={`Version of ${app.instance_name}`}
          >
            {app.versions.map((v, i) => (
              <option key={v.snapshot_id} value={v.snapshot_id}>
                {i === 0 ? "Latest — " : ""}
                {formatDateTime(v.time)}
              </option>
            ))}
          </Select>
          <Button
            onClick={() => void restore()}
            loading={busy}
            disabled={!snapshot}
          >
            Restore
          </Button>
        </div>
      )}
      {error && <p className="text-sm text-danger">{error}</p>}
    </li>
  );
}

/** Home page: install an app from any of its backups instead of from the store. */
export function AddFromBackupButton() {
  const [open, setOpen] = useState(false);
  const res = useApi<BackedUpApps>(
    open ? "backed-up-apps" : null,
    "/api/backups/apps",
    { pollMs: 5_000 },
  );
  const data = res.data;
  return (
    <>
      <Button variant="secondary" size="sm" onClick={() => setOpen(true)}>
        <History className="h-4 w-4" />
        Add from backup
      </Button>
      <Sheet
        open={open}
        onClose={() => setOpen(false)}
        title="Add from backup"
        subtitle="Bring an app back with its settings and files, as they were at the moment you pick."
        wide
      >
        {res.loading && (
          <p className="text-sm text-fg-muted">Reading your backups…</p>
        )}
        {res.error && !data && (
          <p className="text-sm text-danger">
            Your backups could not be read: {res.error}
          </p>
        )}
        {data && !data.configured && (
          <p className="text-sm text-fg-muted">
            Backups are not turned on yet.
          </p>
        )}
        {data && data.configured && data.apps.length === 0 && (
          <p className="text-sm text-fg-muted">
            No app has been backed up yet.
          </p>
        )}
        {data && data.apps.length > 0 && (
          <ul>
            {data.apps.map((app) => (
              <AppRow key={app.namespace} app={app} />
            ))}
          </ul>
        )}
      </Sheet>
    </>
  );
}
