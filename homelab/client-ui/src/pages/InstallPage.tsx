import { useEffect, useMemo, useRef, useState } from "react";
import {
  Link,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router-dom";
import { ArrowLeft, Check, ExternalLink } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Card } from "@/components/ui/card";
import { Select } from "@/components/ui/input";
import { Banner, Spinner } from "@/components/ui/feedback";
import { api, streamEvents } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import { generateSecret } from "@/lib/format";
import Form from "@rjsf/core";
import type { RJSFSchema } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { templates, widgets } from "@/components/form/registry";
import {
  addressTakenBy,
  copiesDataByDefault,
  installBlocker,
  installOrigin,
  installSource,
  keepsSourceAddress,
  phaseFrom,
  snapshotNamespace,
  instanceNameFor,
} from "@/lib/install";
import { AppIconTile } from "@/components/AppIcon";
import { taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type {
  AppDefinition,
  AppInfo,
  CatalogApp,
  DomainResponse,
} from "@/types/apps";

interface SchemaProp {
  type?: string;
  title?: string;
  default?: unknown;
  description?: string;
  format?: string;
  enum?: string[];
  minLength?: number;
  maxLength?: number;
}

interface ConfigSchema {
  properties?: Record<string, SchemaProp>;
  required?: string[];
}

function configSchema(schema: object | undefined): ConfigSchema {
  if (!schema) return {};
  const s = schema as ConfigSchema & { properties?: { config?: ConfigSchema } };
  const nested = s.properties?.config;
  if (nested && typeof nested === "object" && "properties" in nested) {
    return nested;
  }
  return s.properties ? s : {};
}

export function InstallPage() {
  const { appId } = useParams<{ appId: string }>();
  const navigate = useNavigate();

  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const domain = useApi<DomainResponse>("domain", "/api/tunnel/domain");
  const apps = useApi<AppInfo[]>("apps", "/api/apps");

  const cached = catalog.data?.find((a) => a.id === appId);

  const [fresh, setFresh] = useState<CatalogApp | null>(null);
  useEffect(() => {
    if (!appId) return;
    let cancelled = false;
    void (async () => {
      try {
        const r = await api.post<{ app: CatalogApp | null }>(
          `/api/apps/catalog/${appId}/refresh`,
        );
        if (!cancelled && r?.app) setFresh(r.app);
        // eslint-disable-next-line no-empty
      } catch {}
    })();
    return () => {
      cancelled = true;
    };
  }, [appId]);

  const app = fresh ?? cached;

  const [params] = useSearchParams();
  const origin = useMemo(() => installOrigin(params), [params]);
  const [sourceDef, setSourceDef] = useState<AppDefinition | null>(null);
  const [copyData, setCopyData] = useState(copiesDataByDefault(origin.mode));
  const [snapshots, setSnapshots] = useState<
    { id: string; time: string }[] | null
  >(null);
  const [snapshot, setSnapshot] = useState(origin.snapshot ?? "");

  useEffect(() => {
    if (origin.mode === "fresh") return;
    let cancelled = false;
    const url =
      origin.mode === "duplicate"
        ? `/api/apps/${origin.fromInstance}/definition`
        : `/api/backups/apps/${origin.namespace}/definition?snapshot_id=${encodeURIComponent(origin.snapshot ?? "")}`;
    void api
      .get<AppDefinition>(url)
      .then((d) => {
        if (!cancelled) setSourceDef(d);
      })
      .catch(() => {
        if (!cancelled) setSourceDef(null);
      });
    return () => {
      cancelled = true;
    };
  }, [origin]);

  const backupNamespace = snapshotNamespace(origin);
  useEffect(() => {
    if (!backupNamespace || !copyData) return;
    let cancelled = false;
    void api
      .get<{ snapshots?: { id: string; time: string }[] }>(
        `/api/backups/snapshots?namespace=${encodeURIComponent(backupNamespace)}`,
      )
      .then((d) => {
        if (cancelled) return;
        const list = (d.snapshots ?? [])
          .slice()
          .sort(
            (a, b) => new Date(b.time).getTime() - new Date(a.time).getTime(),
          );
        setSnapshots(list);
        setSnapshot((s) => s || list[0]?.id || "");
      })
      .catch(() => {
        if (!cancelled) setSnapshots([]);
      });
    return () => {
      cancelled = true;
    };
  }, [backupNamespace, copyData]);

  const schema = useMemo(() => configSchema(app?.schema), [app?.schema]);
  const required = useMemo(
    () => new Set(schema.required ?? []),
    [schema.required],
  );

  const [installing, setInstalling] = useState(false);
  const [phase, setPhase] = useState("Getting ready");
  const [log, setLog] = useState<string[]>([]);
  const [showLog, setShowLog] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  const installedOfThisApp = (apps.data ?? []).filter(
    (a) => a.app_id === appId,
  );
  const instanceName = instanceNameFor(origin.mode, appId ?? "", sourceDef);

  const addressKey = useMemo(
    () =>
      Object.entries(schema.properties ?? {}).find(
        ([, p]) => p.format === "tunnel",
      )?.[0],
    [schema.properties],
  );

  const rjsfSchema = useMemo(
    () => ({ type: "object", ...schema }) as RJSFSchema,
    [schema],
  );

  const rjsfUiSchema = useMemo(() => {
    const chartUi = (app?.uischema ?? {}) as Record<string, unknown>;
    const ui: Record<string, unknown> = { ...chartUi };
    if (addressKey) {
      const existing = (ui[addressKey] ?? {}) as Record<string, unknown>;
      ui[addressKey] = {
        ...existing,
        "ui:options": {
          ...((existing["ui:options"] as object) ?? {}),
          domain: domain.data?.domain ?? "",
        },
      };
    }
    return ui;
  }, [app?.uischema, addressKey, domain.data?.domain]);

  const [formData, setFormData] = useState<Record<string, unknown>>({});
  const seeded = useRef<string | null>(null);
  useEffect(() => {
    if (!appId || !schema.properties) return;
    const key = `${appId}|${sourceDef ? "source" : "new"}`;
    if (seeded.current === key) return;
    seeded.current = key;
    const seed: Record<string, unknown> = sourceDef
      ? { ...(sourceDef.config as Record<string, unknown>) }
      : {};
    for (const [name, prop] of Object.entries(schema.properties)) {
      const widget = ((
        app?.uischema as Record<string, Record<string, unknown>>
      )?.[name] ?? {})["ui:widget"];
      if (widget === "PasswordWidget") {
        if (!sourceDef) {
          seed[name] = generateSecret(Math.max(24, prop.minLength ?? 0));
        }
      } else if (seed[name] === undefined && prop.default !== undefined) {
        seed[name] = prop.default;
      }
    }
    if (sourceDef && addressKey && !keepsSourceAddress(origin.mode)) {
      delete seed[addressKey];
    }
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setFormData(seed);
  }, [
    appId,
    schema.properties,
    app?.uischema,
    sourceDef,
    addressKey,
    origin.mode,
  ]);

  const values = useMemo(() => {
    if (!addressKey) return formData;
    return formData[addressKey]
      ? formData
      : { ...formData, [addressKey]: instanceName };
  }, [formData, addressKey, instanceName]);

  const generatedSecrets = useMemo<[string, string][]>(() => {
    const ui = (app?.uischema ?? {}) as Record<string, Record<string, unknown>>;
    return Object.entries(schema.properties ?? {})
      .filter(([n]) => ui[n]?.["ui:widget"] === "PasswordWidget")
      .map(([n, p]) => [n, p.title ?? n]);
  }, [app?.uischema, schema.properties]);

  const subdomain =
    addressKey && typeof values[addressKey] === "string"
      ? (values[addressKey] as string)
      : "";
  const fullUrl =
    subdomain && domain.data?.domain
      ? `https://${subdomain}.${domain.data.domain}`
      : null;

  const addressClash = addressTakenBy(subdomain, apps.data ?? []);
  const blocker = installBlocker({
    instanceName,
    addressTakenBy: addressClash,
    requiredMissing: [...required].some((n) => !String(values[n] ?? "").trim()),
    withData: copyData,
    needsBackup: origin.mode === "restore",
    snapshot,
    snapshotsLoaded: snapshots !== null,
    snapshotCount: snapshots?.length ?? 0,
  });

  async function install() {
    if (!app) return;
    setInstalling(true);
    setError(null);
    setLog([]);
    setPhase("Getting ready");

    const payload = Object.fromEntries(
      Object.entries(values).filter(([name, v]) => {
        if (v !== "") return true;
        return schema.properties?.[name]?.default !== undefined;
      }),
    );

    const source = installSource(origin, copyData, snapshot);

    const result = await streamEvents(
      `/api/apps/${app.id}`,
      {
        method: "POST",
        body: JSON.stringify({
          instance_name: instanceName,
          config: payload,
          source,
        }),
      },
      (line) => {
        setLog((l) => [...l, line]);
        const next = phaseFrom(line);
        if (next) setPhase(next);
      },
    );

    setInstalling(false);
    if (result.ok) setDone(true);
    else {
      setError(result.error ?? "Something went wrong during the install.");
      setShowLog(true);
    }
  }

  if (catalog.loading && !app) {
    return (
      <Page>
        <div className="flex justify-center py-20">
          <Spinner />
        </div>
      </Page>
    );
  }

  if (!app) {
    return (
      <Page title="App not found">
        <p className="text-sm text-fg-muted">
          That app is not in the catalog any more.
        </p>
        <Link
          to="/add"
          className={cn(buttonClass({ variant: "secondary" }), "mt-4")}
        >
          Back to apps
        </Link>
      </Page>
    );
  }

  if (done) {
    return (
      <Page>
        <div className="flex flex-col items-center py-10 text-center">
          <div className="mb-5 flex h-16 w-16 items-center justify-center rounded-tile bg-success-soft">
            <Check className="h-8 w-8 text-success" />
          </div>
          <h1 className="text-2xl font-semibold text-fg">
            {app.name} is ready
          </h1>
          <p className="mt-2 max-w-sm text-sm text-fg-muted">
            It may take another minute to finish starting the first time.
          </p>

          {generatedSecrets.length > 0 && (
            <Card className="mt-6 w-full max-w-md p-5 text-left">
              <p className="mb-3 text-sm font-medium text-fg">
                Save these before you leave this page
              </p>
              <div className="space-y-3">
                {generatedSecrets.map(([name, title]) => (
                  <div key={name}>
                    <div className="text-xs text-fg-muted">{title}</div>
                    <code className="block break-all font-mono text-sm text-fg">
                      {String(values[name] ?? "")}
                    </code>
                  </div>
                ))}
              </div>
            </Card>
          )}

          <div className="mt-7 flex w-full max-w-md flex-col gap-2 sm:flex-row">
            {fullUrl && (
              <a
                href={fullUrl}
                target="_blank"
                rel="noopener noreferrer"
                className={cn(buttonClass(), "flex-1")}
              >
                Open {app.name}
                <ExternalLink className="h-4 w-4" />
              </a>
            )}
            <Button
              variant="secondary"
              className="flex-1"
              onClick={() => navigate("/")}
            >
              Back to my apps
            </Button>
          </div>
        </div>
      </Page>
    );
  }

  if (installing) {
    return (
      <Page>
        <div className="flex flex-col items-center py-14 text-center">
          <AppIconTile
            appId={app.id}
            icon={app.icon}
            name={app.name}
            className="mb-6"
          />
          <h1 className="font-display text-2xl text-fg">
            Setting up {app.name}
          </h1>
          <p className="mt-2 text-sm text-fg-muted">{phase}…</p>

          <div className="mt-6 h-1.5 w-full max-w-xs overflow-hidden rounded-full bg-surface-2">
            <div className="h-full w-1/3 animate-pulse rounded-full bg-primary" />
          </div>

          <p className="mt-6 max-w-sm text-sm text-fg-muted">
            This usually takes a minute or two. You can leave this page — it
            keeps going.
          </p>

          <button
            onClick={() => setShowLog((s) => !s)}
            className="mt-6 text-sm text-fg-subtle underline underline-offset-2 hover:text-fg"
          >
            {showLog ? "Hide" : "Show"} technical details
          </button>
          {showLog && (
            <pre className="mt-3 max-h-64 w-full overflow-auto rounded-xl bg-surface-2 p-3 text-left font-mono text-xs leading-relaxed text-fg-muted">
              {log.join("\n")}
            </pre>
          )}
        </div>
      </Page>
    );
  }

  return (
    <Page>
      <Link
        to="/add"
        className="mb-5 inline-flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
      >
        <ArrowLeft className="h-4 w-4" />
        All apps
      </Link>

      <div className="mb-7 flex items-center gap-4">
        <AppIconTile appId={app.id} icon={app.icon} name={app.name} />
        <div className="min-w-0">
          <h1 className="font-display text-3xl text-fg">{app.name}</h1>
          <p className="mt-0.5 text-sm text-fg-muted">{taglineFor(app)}</p>
          {}
          {app.home && (
            <a
              href={app.home}
              target="_blank"
              rel="noreferrer noopener"
              className="mt-1 inline-flex items-center gap-1 text-sm text-fg-subtle transition-colors hover:text-primary hover:underline"
            >
              <ExternalLink className="h-3.5 w-3.5" />
              Visit the project's website
            </a>
          )}
        </div>
      </div>

      {origin.mode === "duplicate" && (
        <Banner
          tone="info"
          title={`Duplicating ${sourceDef?.instance_name ?? origin.fromInstance}`}
          className="mb-5"
        >
          This creates a separate app from the same chart and settings, with its
          own name, storage and web address.{" "}
          {copyData
            ? "Its files are copied from the app itself."
            : "It starts empty — none of its files are copied."}
        </Banner>
      )}
      {origin.mode === "restore" && (
        <Banner
          tone="info"
          title={`Restoring ${sourceDef?.instance_name ?? origin.namespace}`}
          className="mb-5"
        >
          Its settings come back from the backup, and you can change any of them
          here before it is installed.{" "}
          {copyData
            ? "Its files come back from the backup you pick below."
            : "It starts empty — none of its files come back."}
        </Banner>
      )}

      {installedOfThisApp.length > 0 && origin.mode === "fresh" && (
        <Banner
          tone="info"
          title={
            installedOfThisApp.length === 1
              ? `You already have ${app.name}`
              : `You already have ${installedOfThisApp.length} copies of ${app.name}`
          }
          className="mb-5"
        >
          This adds another, completely separate one — its own storage, its own
          login, its own web address. Nothing about the existing{" "}
          {installedOfThisApp.length === 1 ? "one" : "ones"} changes.
        </Banner>
      )}

      {error && (
        <Banner
          tone="error"
          title="The install did not finish"
          className="mb-5"
        >
          {error}
        </Banner>
      )}

      {app.repo !== "official" && (
        <Banner
          tone="warning"
          title={`This app comes from "${app.repo}"`}
          className="mb-5"
        >
          You added this source yourself. Apps from outside the official catalog
          can do anything on your machines, so only install ones you trust.
        </Banner>
      )}

      <Card className="divide-y divide-border">
        {}
        {}
        <div className="p-5">
          <Form
            schema={rjsfSchema}
            uiSchema={rjsfUiSchema}
            formData={formData}
            validator={validator}
            widgets={widgets}
            templates={templates}
            liveValidate={false}
            showErrorList={false}
            onChange={(e) => setFormData(e.formData ?? {})}
          >
            {}
            <></>
          </Form>
        </div>

        {origin.mode !== "fresh" && (
          <div className="space-y-3 p-5">
            <label className="flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                checked={copyData}
                onChange={(e) => setCopyData(e.target.checked)}
                className="accent-primary"
              />
              {origin.mode === "duplicate"
                ? "Copy this app’s files too"
                : "Bring this app’s files back too"}
            </label>
            {copyData &&
              origin.mode === "restore" &&
              (snapshots === null ? (
                <p className="text-xs text-fg-muted">Looking for backups…</p>
              ) : snapshots.length === 0 ? (
                <p className="text-xs text-fg-muted">
                  There is no backup of this app yet, so there is nothing to
                  copy. Install it empty, or back it up first.
                </p>
              ) : (
                <Select
                  value={snapshot}
                  onChange={(e) => setSnapshot(e.target.value)}
                  aria-label="Backup to copy from"
                >
                  {snapshots.map((s, i) => (
                    <option key={s.id} value={s.id}>
                      {i === 0 ? "Latest — " : ""}
                      {new Date(s.time).toLocaleString()}
                    </option>
                  ))}
                </Select>
              ))}
          </div>
        )}
      </Card>

      <div className="mt-7">
        <Button
          full
          size="lg"
          onClick={() => void install()}
          disabled={blocker !== null}
        >
          Install {app.name}
        </Button>
        {blocker && (
          <p className="mt-2 text-center text-sm text-fg-muted">{blocker}</p>
        )}
      </div>
    </Page>
  );
}
