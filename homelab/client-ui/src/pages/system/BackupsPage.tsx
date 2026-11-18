import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { Banner, Skeleton } from "@/components/ui/feedback";
import { Row, RowAction, Section } from "@/components/ui/list";
import { AppIconTile } from "@/components/AppIcon";
import { StatusDot } from "@/components/ui/badge";
import { RecoveryKeyOverlay } from "@/components/RecoveryKeyOverlay";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import {
  backupPagePath,
  backupServices,
  needsAttention,
  STALE_AFTER_HOURS,
  tileLabel,
  tileTone,
} from "@/lib/backups";
import type {
  BackedUpService,
  BackupService,
  ProtectedApp,
} from "@/lib/backups";
import type { CatalogApp } from "@/types/apps";
import { Collapse, RollingNumber, Swap } from "@/components/motion";
import { cn } from "@/lib/utils";

interface ProtectedApps {
  configured: boolean;
  apps: ProtectedApp[];
}

interface BackedUpServices {
  configured: boolean;
  apps: BackedUpService[];
}

function BackupTile({
  service,
  icon,
  now,
}: {
  service: BackupService;
  icon?: string;
  now: number;
}) {
  const tone = tileTone(service, now);
  const label = tileLabel(service, now);
  return (
    <Link
      to={backupPagePath(service.namespace)}
      className={cn(
        "group flex flex-col items-center rounded-card p-3 transition-[background-color,opacity,transform] duration-300 hover:bg-surface active:scale-[0.97]",
        !service.installed && "opacity-70",
      )}
    >
      <div className="relative mb-3">
        <AppIconTile
          appId={service.appId ?? undefined}
          icon={icon}
          name={service.name}
          className="shadow-[var(--shadow-card)] transition-all duration-200 group-hover:-translate-y-0.5 group-hover:shadow-[var(--shadow-lift)]"
        />
        <StatusDot
          tone={tone}
          pulse={tone === "busy"}
          className="absolute -right-0.5 -top-0.5"
        />
      </div>
      <span className="line-clamp-1 text-sm font-medium text-fg">
        {service.name}
      </span>
      <Swap
        id={label}
        className="mt-0.5 line-clamp-2 text-balance text-center text-xs text-fg-muted"
      >
        {label}
      </Swap>
    </Link>
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
  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
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
        your home. Open one to see its copies through time and bring any of them
        back.
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

          {services.length === 0 ? (
            <Section title="Services">
              <Row
                label="Nothing to back up yet"
                detail="Once you add a service, it is backed up automatically."
              />
            </Section>
          ) : (
            <section className="mt-8">
              <h2 className="mb-2 px-1 text-sm font-semibold text-fg-muted">
                Services
              </h2>
              <div className="grid animate-fade-in grid-cols-3 gap-2 sm:grid-cols-4 md:grid-cols-6">
                {services.map((s) => (
                  <BackupTile
                    key={s.namespace}
                    service={s}
                    now={now}
                    icon={catalog.data?.find((c) => c.id === s.appId)?.icon}
                  />
                ))}
              </div>
            </section>
          )}

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
