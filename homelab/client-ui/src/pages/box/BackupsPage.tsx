import { useCallback, useEffect, useState } from "react";
import {
  AlertTriangle,
  ChevronRight,
  Database,
  KeyRound,
  RotateCcw,
} from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Banner, Skeleton } from "@/components/ui/feedback";
import { Sheet } from "@/components/ui/sheet";
import { AppIcon } from "@/components/AppIcon";
import { RestorePointList } from "@/components/RestorePoints";
import { useRestorePoints } from "@/lib/useRestorePoints";
import { RecoveryKeyOverlay } from "@/components/RecoveryKeyOverlay";
import { api } from "@/lib/api";
import { formatDateTime } from "@/lib/format";
import { useApi } from "@/lib/useResource";
import {
  isBusy,
  needsAttention,
  protectionLabel,
  protectionState,
  protectionTone,
} from "@/lib/backups";
import type { ProtectedApp } from "@/lib/backups";

const STALE_AFTER_HOURS = 36;

interface ProtectedApps {
  configured: boolean;
  apps: ProtectedApp[];
}

const TONE_CLASS: Record<string, string> = {
  success: "text-success",
  info: "text-primary",
  danger: "text-danger",
  muted: "text-fg-subtle",
};

function AppRow({
  app,
  onOpen,
  onBackupNow,
}: {
  app: ProtectedApp;
  onOpen: () => void;
  onBackupNow: () => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const state = protectionState(app);
  const working = isBusy(state);

  async function backupNow(e: React.MouseEvent) {
    e.stopPropagation();
    setBusy(true);
    try {
      await onBackupNow();
    } finally {
      setBusy(false);
    }
  }

  return (
    <li>
      <button
        onClick={onOpen}
        className="flex w-full items-center gap-3 px-1 py-3 text-left transition-colors hover:bg-surface-2"
      >
        <AppIcon
          appId={app.app_id}
          name={app.instance_name}
          className="h-8 w-8"
        />
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium text-fg">
            {app.instance_name}
          </div>
          <div className="flex flex-wrap items-center gap-x-2 text-xs">
            <span className={TONE_CLASS[protectionTone(state)]}>
              {protectionLabel(state)}
            </span>
            {app.last_ok_at && (
              <span className="text-fg-muted">
                · {formatDateTime(app.last_ok_at)}
              </span>
            )}
          </div>
          {app.error && (
            <p className="mt-1 line-clamp-2 text-xs text-danger">{app.error}</p>
          )}
        </div>
        {app.enabled && (
          <Button
            size="sm"
            variant="ghost"
            loading={busy || working}
            disabled={busy || working}
            onClick={(e) => void backupNow(e)}
          >
            <RotateCcw className="h-3.5 w-3.5" />
            Save now
          </Button>
        )}
        <ChevronRight className="h-4 w-4 shrink-0 text-fg-subtle" />
      </button>
    </li>
  );
}

function AppRestoreSheet({
  app,
  onClose,
}: {
  app: ProtectedApp | null;
  onClose: () => void;
}) {
  const { points, error } = useRestorePoints(app?.namespace ?? null);
  if (!app) return null;
  return (
    <Sheet
      open
      onClose={onClose}
      title={app.instance_name}
      subtitle="Pick the moment you want back. You will see the install screen before anything is created, so you can change its name and web address first."
      wide
    >
      <RestorePointList
        appId={app.app_id}
        namespace={app.namespace}
        points={points}
        error={error}
        emptyHint="This app has not been backed up yet. Use “Save now” and it will appear here."
      />
    </Sheet>
  );
}

function EnableCard({ onEnable }: { onEnable: () => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handle() {
    setBusy(true);
    setError(null);
    try {
      await onEnable();
    } catch (e) {
      setError(e instanceof Error ? e.message : "That did not work.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardContent className="flex items-start gap-3 py-5">
        <div className="mt-0.5 shrink-0 rounded-md bg-warning-soft p-1.5">
          <Database className="h-4 w-4 text-warning" strokeWidth={1.75} />
        </div>
        <div className="flex-1">
          <div className="flex flex-wrap items-center justify-between gap-4">
            <div>
              <p className="text-sm font-medium text-fg">Backups are off</p>
              <p className="mt-0.5 text-xs text-fg-muted">
                Turn them on and every app is copied, encrypted, to storage
                outside your home.
              </p>
            </div>
            <Button onClick={() => void handle()} loading={busy} size="sm">
              Turn on backups
            </Button>
          </div>
          {error && <p className="mt-2 text-xs text-danger">{error}</p>}
        </div>
      </CardContent>
    </Card>
  );
}

export function BackupsPage() {
  const protectedApps = useApi<ProtectedApps>(
    "protected-apps",
    "/api/backups/protected",
    { pollMs: 10_000 },
  );
  const [open, setOpen] = useState<ProtectedApp | null>(null);
  const [recoveryKey, setRecoveryKey] = useState<string | null>(null);
  const [recoveryMandatory, setRecoveryMandatory] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  const showRecoveryKey = useCallback(async (mandatory: boolean) => {
    try {
      const data = await api.get<{
        configured: boolean;
        recovery_key?: string;
      }>("/api/backups/recovery-key");
      if (data.configured && data.recovery_key) {
        setRecoveryKey(data.recovery_key);
        setRecoveryMandatory(mandatory);
      }
      // eslint-disable-next-line no-empty
    } catch {}
  }, []);

  async function enable() {
    await api.post("/api/backups/s3/enable");
    await protectedApps.refresh();
    await showRecoveryKey(true);
  }

  const backupNow = useCallback(
    async (namespace: string) => {
      setActionError(null);
      try {
        await api.post(
          `/api/backups/apps/${encodeURIComponent(namespace)}/run-now`,
        );
        await protectedApps.refresh();
      } catch (e) {
        setActionError(
          e instanceof Error ? e.message : "That backup could not be started.",
        );
      }
    },
    [protectedApps],
  );

  const data = protectedApps.data;
  const apps = data?.apps ?? [];
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);
  const attention = needsAttention(apps, now, STALE_AFTER_HOURS);

  return (
    <div className="max-w-3xl space-y-6">
      {recoveryKey && (
        <RecoveryKeyOverlay
          recoveryKey={recoveryKey}
          mandatory={recoveryMandatory}
          onClose={() => setRecoveryKey(null)}
        />
      )}

      <div className="flex flex-wrap items-start justify-between gap-4">
        <p className="max-w-lg text-sm text-fg-muted">
          Every app is copied on its own schedule, encrypted, to storage outside
          your home. Open one to bring it back as it was at any point in time.
        </p>
        {data?.configured && (
          <Button
            onClick={() => void showRecoveryKey(false)}
            variant="ghost"
            size="sm"
          >
            <KeyRound className="h-3.5 w-3.5" />
            Recovery key
          </Button>
        )}
      </div>

      {actionError && (
        <Banner tone="error" title="That did not start">
          {actionError}
        </Banner>
      )}

      {protectedApps.loading && !data ? (
        <Card>
          <CardContent className="py-5">
            <Skeleton className="h-14 w-full" />
          </CardContent>
        </Card>
      ) : !data?.configured ? (
        <EnableCard onEnable={enable} />
      ) : (
        <>
          {attention.length > 0 && (
            <Banner
              tone="warning"
              title={
                attention.length === 1
                  ? `${attention[0].instance_name} has no recent backup`
                  : `${attention.length} apps have no recent backup`
              }
            >
              Anything changed in {attention.length === 1 ? "it" : "them"} since
              the last good copy is not saved anywhere else yet. Use “Save now”,
              or open the app to see what went wrong.
            </Banner>
          )}

          {apps.length === 0 ? (
            <Card>
              <CardContent className="py-5">
                <p className="text-sm text-fg-muted">
                  You have no apps installed yet. Once you add one it is backed
                  up automatically.
                </p>
              </CardContent>
            </Card>
          ) : (
            <Card>
              <CardContent className="py-1">
                <ul className="divide-y divide-border">
                  {apps.map((app) => (
                    <AppRow
                      key={app.namespace}
                      app={app}
                      onOpen={() => setOpen(app)}
                      onBackupNow={() => backupNow(app.namespace)}
                    />
                  ))}
                </ul>
              </CardContent>
            </Card>
          )}

          <p className="flex items-start gap-2 text-xs text-fg-subtle">
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            Your backups can only be read with your recovery key. Keep a copy
            somewhere other than this machine.
          </p>
        </>
      )}

      <AppRestoreSheet app={open} onClose={() => setOpen(null)} />
    </div>
  );
}
