import { useState } from "react";
import { ChevronRight, History } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Sheet } from "@/components/ui/sheet";
import { useApi } from "@/lib/useResource";
import { RestorePointList } from "@/components/RestorePoints";
import { useRestorePoints } from "@/lib/useRestorePoints";

interface BackedUpApps {
  configured: boolean;
  apps: {
    namespace: string;
    instance_name: string;
    installed: boolean;
    versions: { snapshot_id: string; time: string }[];
  }[];
}

type BackedUpApp = BackedUpApps["apps"][number];

function AppPoints({ app, onBack }: { app: BackedUpApp; onBack: () => void }) {
  const { points, error } = useRestorePoints(app.namespace);
  return (
    <div className="space-y-3">
      <button
        onClick={onBack}
        className="text-sm text-fg-muted underline underline-offset-2 hover:text-fg"
      >
        All backed-up apps
      </button>
      {app.installed && (
        <p className="text-xs text-fg-muted">
          Restoring here adds a separate copy alongside the one you already run.
          To put this backup back into the app you are running, open it and use
          Restore there.
        </p>
      )}
      <RestorePointList
        namespace={app.namespace}
        points={points}
        error={error}
        emptyHint="No backup of this app could be read."
      />
    </div>
  );
}

export function AddFromBackupButton() {
  const [open, setOpen] = useState(false);
  const [chosen, setChosen] = useState<BackedUpApp | null>(null);
  const res = useApi<BackedUpApps>(
    open ? "backed-up-apps" : null,
    "/api/backups/apps",
  );
  const data = res.data;

  function close() {
    setOpen(false);
    setChosen(null);
  }

  return (
    <>
      <Button variant="secondary" size="sm" onClick={() => setOpen(true)}>
        <History className="h-4 w-4" />
        Add from backup
      </Button>
      <Sheet
        open={open}
        onClose={close}
        title={chosen ? chosen.instance_name : "Add from backup"}
        subtitle={
          chosen
            ? "Pick the moment you want back."
            : "Bring an app back as it was, with its settings and files."
        }
        wide
      >
        {chosen ? (
          <AppPoints app={chosen} onBack={() => setChosen(null)} />
        ) : (
          <>
            {res.loading && !data && (
              <p className="text-sm text-fg-muted">Reading your backups…</p>
            )}
            {res.error && !data && (
              <p className="text-sm text-danger">
                Your backups could not be read: {res.error}
              </p>
            )}
            {data && !data.configured && (
              <p className="text-sm text-fg-muted">Backups are not on yet.</p>
            )}
            {data?.configured && data.apps.length === 0 && (
              <p className="text-sm text-fg-muted">
                No app has been backed up yet.
              </p>
            )}
            {data && data.apps.length > 0 && (
              <ul className="divide-y divide-border">
                {data.apps.map((app) => (
                  <li key={app.namespace}>
                    <button
                      onClick={() => setChosen(app)}
                      className="flex w-full items-center justify-between gap-3 py-3 text-left"
                    >
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium text-fg">
                          {app.instance_name}
                        </div>
                        <div className="text-xs text-fg-muted">
                          {app.versions.length} point
                          {app.versions.length === 1 ? "" : "s"} in time
                          {app.installed ? " · already installed" : ""}
                        </div>
                      </div>
                      <ChevronRight className="h-4 w-4 shrink-0 text-fg-subtle" />
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </Sheet>
    </>
  );
}
