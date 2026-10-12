import { useCallback, useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { Banner, Skeleton } from "@/components/ui/feedback";
import { Row, RowAction, Section } from "@/components/ui/list";
import { AppIcon } from "@/components/AppIcon";
import { RecoveryKeyOverlay } from "@/components/RecoveryKeyOverlay";
import { api } from "@/lib/api";
import { formatDateTime } from "@/lib/format";
import { useApi } from "@/lib/useResource";
import {
  needsAttention,
  protectionLabel,
  protectionState,
  protectionTone,
} from "@/lib/backups";
import type { ProtectedApp } from "@/lib/backups";
import { Collapse, RollingNumber, Swap } from "@/components/motion";
import { cn } from "@/lib/utils";

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

function AppRow({ app }: { app: ProtectedApp }) {
  const navigate = useNavigate();
  const state = protectionState(app);
  return (
    <Row
      onClick={() => navigate(`/app/${app.instance_name}`)}
      label={
        <span className="flex items-center gap-2.5">
          <AppIcon
            appId={app.app_id}
            name={app.instance_name}
            className="h-6 w-6"
          />
          <span className="truncate">{app.instance_name}</span>
        </span>
      }
      detail={
        <>
          <Swap
            id={protectionLabel(state)}
            className={cn(
              "inline-block transition-colors duration-300",
              TONE_CLASS[protectionTone(state)],
            )}
          >
            {protectionLabel(state)}
          </Swap>
          {app.last_ok_at && (
            <span className="font-mono tabular-nums">
              {" · "}
              {formatDateTime(app.last_ok_at)}
            </span>
          )}
          {app.error && (
            <span className="mt-1 line-clamp-2 block text-danger">
              {app.error}
            </span>
          )}
        </>
      }
    />
  );
}

export function BackupsPage() {
  const protectedApps = useApi<ProtectedApps>(
    "protected-apps",
    "/api/backups/protected",
    { pollMs: 10_000 },
  );
  const [recoveryKey, setRecoveryKey] = useState<string | null>(null);
  const [recoveryMandatory, setRecoveryMandatory] = useState(false);
  const [enabling, setEnabling] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const showRecoveryKey = useCallback(async (mandatory: boolean) => {
    setError(null);
    try {
      const data = await api.get<{
        configured: boolean;
        recovery_key?: string;
      }>("/api/backups/recovery-key");
      if (data.configured && data.recovery_key) {
        setRecoveryKey(data.recovery_key);
        setRecoveryMandatory(mandatory);
      }
    } catch (e) {
      setError(
        e instanceof Error ? e.message : "The recovery key could not be read.",
      );
    }
  }, []);

  async function enable() {
    setEnabling(true);
    setError(null);
    try {
      await api.post("/api/backups/s3/enable");
      await protectedApps.refresh();
      await showRecoveryKey(true);
    } catch (e) {
      setError(
        e instanceof Error ? e.message : "Backups could not be turned on.",
      );
    } finally {
      setEnabling(false);
    }
  }

  const data = protectedApps.data;
  const apps = data?.apps ?? [];
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);
  const attention = needsAttention(apps, now, STALE_AFTER_HOURS);

  return (
    <div className="max-w-3xl">
      {recoveryKey && (
        <RecoveryKeyOverlay
          recoveryKey={recoveryKey}
          mandatory={recoveryMandatory}
          onClose={() => setRecoveryKey(null)}
        />
      )}

      <p className="max-w-lg text-sm text-fg-muted">
        Every app is copied with its folders, encrypted, to storage outside your
        home. Open one to back it up now or bring it back as it was.
      </p>

      <Collapse open={Boolean(error)} className="pt-6">
        <Banner tone="error" title="That did not work">
          {error}
        </Banner>
      </Collapse>

      {protectedApps.loading && !data ? (
        <Skeleton className="mt-8 h-32 w-full" />
      ) : !data?.configured ? (
        <Section title="Backups">
          <Row
            label="Backups are off"
            detail="Turn them on and every app is copied, encrypted, to storage outside your home."
            trailing={
              <Button
                onClick={() => void enable()}
                loading={enabling}
                size="sm"
              >
                Turn on backups
              </Button>
            }
          />
        </Section>
      ) : (
        <>
          <Collapse open={!error && attention.length > 0} className="pt-6">
            <Banner
              tone="warning"
              title={
                attention.length === 1 ? (
                  `${attention[0].instance_name} has no recent backup`
                ) : (
                  <>
                    <RollingNumber value={attention.length} /> apps have no
                    recent backup
                  </>
                )
              }
            >
              Anything changed in {attention.length === 1 ? "it" : "them"} since
              the last good copy is not saved anywhere else yet. Open{" "}
              {attention.length === 1 ? "it" : "each one"} to back up now or see
              what went wrong.
            </Banner>
          </Collapse>

          <Section title="Apps">
            {apps.length === 0 ? (
              <Row
                label="Nothing to back up yet"
                detail="Once you add an app, it is backed up automatically."
              />
            ) : (
              apps.map((app) => <AppRow key={app.namespace} app={app} />)
            )}
          </Section>

          <Section title="Recovery key">
            <Row
              label="Recovery key"
              detail="Your backups can only be read with it. Keep a copy somewhere other than this machine."
              trailing={
                <RowAction onClick={() => void showRecoveryKey(false)}>
                  Show
                </RowAction>
              }
            />
          </Section>
        </>
      )}
    </div>
  );
}
