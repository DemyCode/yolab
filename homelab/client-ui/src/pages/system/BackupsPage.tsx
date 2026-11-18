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
  backupPagePath,
  backupServices,
  copiesLabel,
  needsAttention,
  protectionLabel,
  protectionState,
  protectionTone,
  usageLabel,
} from "@/lib/backups";
import type {
  BackedUpService,
  BackupService,
  ProtectedApp,
} from "@/lib/backups";
import { Collapse, RollingNumber, Swap } from "@/components/motion";
import { cn } from "@/lib/utils";

const STALE_AFTER_HOURS = 36;

interface ProtectedApps {
  configured: boolean;
  apps: ProtectedApp[];
}

interface BackedUpServices {
  configured: boolean;
  apps: BackedUpService[];
}

const TONE_CLASS: Record<string, string> = {
  success: "text-success",
  info: "text-primary",
  danger: "text-danger",
  muted: "text-fg-subtle",
};

function ServiceRow({ service }: { service: BackupService }) {
  const navigate = useNavigate();
  const app = service.protection;
  const state = app ? protectionState(app) : null;
  const usage = usageLabel(service.sizeBytes, app?.budget_bytes);
  return (
    <Row
      onClick={() => navigate(backupPagePath(service.namespace))}
      label={
        <span className="flex items-center gap-2.5">
          <AppIcon
            appId={service.appId ?? undefined}
            name={service.name}
            className="h-6 w-6"
          />
          <span className="truncate">{service.name}</span>
          {!service.installed && (
            <span className="rounded-full border border-border px-2 py-0.5 text-xs font-normal text-fg-subtle">
              Removed
            </span>
          )}
        </span>
      }
      detail={
        <>
          {state && (
            <Swap
              id={protectionLabel(state)}
              className={cn(
                "inline-block transition-colors duration-300",
                TONE_CLASS[protectionTone(state)],
              )}
            >
              {protectionLabel(state)}
            </Swap>
          )}
          {app?.last_ok_at && (
            <span className="font-mono tabular-nums">
              {" · "}
              {formatDateTime(app.last_ok_at)}
            </span>
          )}
          <span className="font-mono tabular-nums">
            {state ? " · " : ""}
            {copiesLabel(service.copies)}
          </span>
          {usage && (
            <span
              className={cn(
                "font-mono tabular-nums",
                app?.over_budget && "text-warning",
              )}
            >
              {" · "}
              {usage}
            </span>
          )}
          {app?.error && (
            <span className="mt-1 line-clamp-2 block text-danger">
              {app.error}
            </span>
          )}
          {!app?.error && app?.warning && (
            <span className="mt-1 line-clamp-2 block text-warning">
              {app.warning}
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
  const backedUp = useApi<BackedUpServices>(
    "backed-up-apps",
    "/api/backups/apps",
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
  const services = backupServices(apps, backedUp.data?.apps ?? []);
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
          onAcknowledge={
            recoveryMandatory
              ? () => api.post("/api/backups/recovery-key/ack")
              : undefined
          }
          onClose={() => setRecoveryKey(null)}
        />
      )}

      <p className="max-w-lg text-sm text-fg-muted">
        Every service is copied with its own data, encrypted, to storage outside
        your home. Open one to see its copies through time and bring any of them back.
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
            detail="Turn them on and every service is copied, encrypted, to storage outside your home."
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
                    <RollingNumber value={attention.length} /> services have no
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

          <Section title="Services">
            {services.length === 0 ? (
              <Row
                label="Nothing to back up yet"
                detail="Once you add a service, it is backed up automatically."
              />
            ) : (
              services.map((s) => <ServiceRow key={s.namespace} service={s} />)
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
