import { useCallback, useState } from "react";
import { useNavigate } from "react-router-dom";
import { Button } from "@/components/ui/button";
import { Banner, Skeleton } from "@/components/ui/feedback";
import { Row, RowAction, Section } from "@/components/ui/list";
import { AppIconTile } from "@/components/AppIcon";
import { StatusDot } from "@/components/ui/badge";
import { RecoveryKeyOverlay } from "@/components/RecoveryKeyOverlay";
import { api } from "@/lib/api";
import { formatBytes } from "@/lib/format";
import { useApi } from "@/lib/useResource";
import { useNow } from "@/lib/useNow";
import {
  backupPagePath,
  backupServices,
  byStorage,
  needsAttention,
  serviceDetail,
  serviceTone,
  STALE_AFTER_HOURS,
  storageTotal,
} from "@/lib/backups";
import type {
  BackedUpService,
  BackupService,
  ProtectedApp,
} from "@/lib/backups";
import type { CatalogApp } from "@/types/apps";
import { Collapse, RollingNumber } from "@/components/motion";

interface ProtectedApps {
  configured: boolean;
  apps: ProtectedApp[];
}

interface BackedUpServices {
  configured: boolean;
  apps: BackedUpService[];
}

function ServiceRow({
  service,
  icon,
  now,
}: {
  service: BackupService;
  icon?: string;
  now: number;
}) {
  const navigate = useNavigate();
  const tone = serviceTone(service, now);
  return (
    <Row
      onClick={() => navigate(backupPagePath(service.namespace))}
      label={
        <span className="flex items-center gap-3">
          <span className="relative shrink-0">
            <AppIconTile
              appId={service.appId ?? undefined}
              icon={icon}
              name={service.name}
              size="sm"
            />
            {tone !== "ok" && (
              <StatusDot
                tone={tone}
                pulse={tone === "busy"}
                className="absolute -right-0.5 -top-0.5"
              />
            )}
          </span>
          <span className="min-w-0">
            <span className="block truncate">{service.name}</span>
            <span className="block truncate text-sm font-normal text-fg-muted">
              {serviceDetail(service, now)}
            </span>
          </span>
        </span>
      }
      trailing={
        <span className="shrink-0 font-mono text-sm tabular-nums text-fg-muted">
          {service.sizeBytes != null ? formatBytes(service.sizeBytes) : "—"}
        </span>
      }
    />
  );
}

export function BackupsPage() {
  const navigate = useNavigate();
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
  const now = useNow(60_000);
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
  const services = byStorage(backupServices(apps, backedUp.data?.apps ?? []));
  const installed = services.filter((s) => s.installed);
  const removed = services.filter((s) => !s.installed);
  const total = storageTotal(services);
  const attention = needsAttention(apps, now, STALE_AFTER_HOURS);
  const icon = (s: BackupService) =>
    catalog.data?.find((c) => c.id === s.appId)?.icon;

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
        {total != null ? (
          <>
            <span className="font-mono tabular-nums text-fg">
              {formatBytes(total)}
            </span>{" "}
            of encrypted copies, kept outside your home. You pay for this space,
            so the biggest services are listed first.
          </>
        ) : (
          "Every service is copied with its own data, encrypted, to storage outside your home."
        )}
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
              {attention.length === 1 ? "it" : "each one"} below to back up now
              or see what went wrong.
            </Banner>
          </Collapse>

          <Section title="Services">
            {installed.length === 0 ? (
              <Row
                label="Nothing to back up yet"
                detail="Once you add a service, it is backed up automatically."
              />
            ) : (
              installed.map((s) => (
                <ServiceRow
                  key={s.namespace}
                  service={s}
                  icon={icon(s)}
                  now={now}
                />
              ))
            )}
          </Section>

          {removed.length > 0 && (
            <Section title="Removed services">
              {removed.map((s) => (
                <ServiceRow
                  key={s.namespace}
                  service={s}
                  icon={icon(s)}
                  now={now}
                />
              ))}
            </Section>
          )}

          <Section title="For every service">
            <Row
              label="Defaults for new services"
              detail="How often a new service is backed up and how much space its copies get"
              onClick={() => navigate("/system/settings")}
            />
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
