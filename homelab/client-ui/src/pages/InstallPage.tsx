import { useEffect, useMemo, useRef, useState } from "react";
import {
  Link,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router-dom";
import { ArrowLeft, ExternalLink } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Card } from "@/components/ui/card";
import { Select, Switch } from "@/components/ui/input";
import { Row, Section } from "@/components/ui/list";
import { Banner, Spinner } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import { formatDateTime, generateSecret } from "@/lib/format";
import Form from "@rjsf/core";
import type { RJSFSchema } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { templates, widgets } from "@/components/form/registry";
import {
  copiesDataByDefault,
  installBlocker,
  installOrigin,
  installSource,
  seedForm,
  snapshotNamespace,
  instanceNameFor,
} from "@/lib/install";
import { AppIconTile } from "@/components/AppIcon";
import { configSchemaOf, generatedFields, uiSchemaFor } from "@/lib/schema";
import { taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type {
  AppDefinition,
  AppInfo,
  CatalogApp,
  DomainResponse,
} from "@/types/apps";

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

  const schema = useMemo(() => configSchemaOf(app?.schema), [app?.schema]);
  const required = useMemo(
    () => new Set(schema.required ?? []),
    [schema.required],
  );

  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [started, setStarted] = useState<string | null>(null);

  const installedOfThisApp = (apps.data ?? []).filter(
    (a) => a.app_id === appId,
  );
  const instanceName = instanceNameFor(origin.mode, appId ?? "", sourceDef);

  const rjsfSchema = useMemo(
    () => ({ type: "object", ...schema }) as RJSFSchema,
    [schema],
  );

  const rjsfUiSchema = useMemo(
    () => uiSchemaFor(schema, domain.data?.domain ?? ""),
    [schema, domain.data?.domain],
  );

  const [formData, setFormData] = useState<Record<string, unknown>>({});
  const seeded = useRef<string | null>(null);
  useEffect(() => {
    if (!appId || !schema.properties) return;
    const key = `${appId}|${sourceDef ? "source" : "new"}`;
    if (seeded.current === key) return;
    seeded.current = key;
    const seed = seedForm(
      schema,
      sourceDef ? (sourceDef.config as Record<string, unknown>) : null,
      origin.mode,
      generateSecret,
    );
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setFormData(seed);
  }, [appId, schema, sourceDef, origin.mode]);

  const values = formData;

  const generatedSecrets = useMemo(() => generatedFields(schema), [schema]);

  const blocker = installBlocker({
    instanceName,
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

    const payload = Object.fromEntries(
      Object.entries(values).filter(([name, v]) => {
        if (v !== "") return true;
        return schema.properties?.[name]?.default !== undefined;
      }),
    );

    try {
      const begun = await api.post<{ instance_name: string }>(
        `/api/apps/${app.id}`,
        {
          instance_name: instanceName,
          config: payload,
          source: installSource(origin, copyData, snapshot),
        },
      );
      if (generatedSecrets.length > 0) setStarted(begun.instance_name);
      else navigate(`/app/${begun.instance_name}`);
    } catch (e) {
      setError(e instanceof Error ? e.message : "The install could not start.");
      setInstalling(false);
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

  if (started) {
    return (
      <Page>
        <div className="flex flex-col items-center py-10 text-center">
          <AppIconTile
            appId={app.id}
            icon={app.icon}
            name={app.name}
            className="mb-6"
          />
          <h1 className="font-display text-[1.75rem] leading-tight text-fg">
            Installing {app.name}
          </h1>
          <p className="mt-2 max-w-sm text-sm text-fg-muted">
            It keeps going on your server. It becomes ready on its own, or shows
            “Failed installation” with the reason.
          </p>

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

          <Button
            className="mt-7 w-full max-w-md"
            onClick={() => navigate(`/app/${started}`)}
          >
            Go to {app.name}
          </Button>
        </div>
      </Page>
    );
  }

  const sourceName =
    origin.mode === "duplicate"
      ? (sourceDef?.instance_name ?? origin.fromInstance)
      : (sourceDef?.instance_name ?? origin.namespace);

  const notice = error ? (
    <Banner tone="error" title="The install could not start">
      {error}
    </Banner>
  ) : app.repo !== "official" ? (
    <Banner tone="warning" title={`This app comes from "${app.repo}"`}>
      You added this source yourself. Apps from outside the official catalog can
      do anything on your machines, so only install ones you trust.
    </Banner>
  ) : null;

  return (
    <Page>
      <Link
        to="/add"
        className="mb-5 inline-flex items-center gap-1.5 rounded-control text-sm text-fg-muted hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary"
      >
        <ArrowLeft className="h-4 w-4" />
        All apps
      </Link>

      <header className="flex items-center gap-4">
        <AppIconTile appId={app.id} icon={app.icon} name={app.name} />
        <div className="min-w-0 flex-1">
          <h1 className="font-display text-[1.75rem] leading-tight text-fg md:text-4xl">
            {origin.mode === "duplicate"
              ? `Duplicate ${sourceName ?? app.name}`
              : origin.mode === "restore"
                ? `Restore ${sourceName ?? app.name}`
                : app.name}
          </h1>
          <p className="mt-1 text-sm text-fg-muted">
            {origin.mode === "duplicate"
              ? "A separate app from the same settings, with its own address and storage."
              : origin.mode === "restore"
                ? "Its settings come back from the backup; change any of them before it is installed."
                : installedOfThisApp.length > 0
                  ? `${taglineFor(app)} You already have ${installedOfThisApp.length === 1 ? "one" : installedOfThisApp.length}; this adds a separate one.`
                  : taglineFor(app)}
          </p>
          {app.home && origin.mode === "fresh" && (
            <a
              href={app.home}
              target="_blank"
              rel="noreferrer noopener"
              className="mt-1 inline-flex items-center gap-1 rounded-control text-sm text-fg-subtle transition-colors hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary"
            >
              The project&rsquo;s website
              <ExternalLink className="h-3.5 w-3.5" />
            </a>
          )}
        </div>
      </header>

      {notice && <div className="mt-6">{notice}</div>}

      <section className="mt-8">
        <h2 className="mb-2 px-1 text-sm font-semibold text-fg-muted">
          Settings
        </h2>
        <Card className="p-5">
          <Form
            schema={rjsfSchema}
            uiSchema={rjsfUiSchema}
            formData={formData}
            formContext={{ formData }}
            validator={validator}
            widgets={widgets}
            templates={templates}
            liveValidate={false}
            showErrorList={false}
            onChange={(e) => setFormData(e.formData ?? {})}
          >
            <></>
          </Form>
        </Card>
      </section>

      {origin.mode !== "fresh" && (
        <Section title="Files">
          <Row
            label={
              origin.mode === "duplicate"
                ? "Copy this app’s files too"
                : "Bring its files back too"
            }
            detail={
              copyData
                ? origin.mode === "duplicate"
                  ? "Copied straight from the app, as they are now."
                  : "From the backup you pick below."
                : "It starts empty."
            }
            trailing={
              <Switch
                checked={copyData}
                onChange={setCopyData}
                label={
                  origin.mode === "duplicate"
                    ? "Copy this app’s files too"
                    : "Bring its files back too"
                }
              />
            }
          />
          {copyData && origin.mode === "restore" && (
            <Row
              label="From the backup of"
              detail={
                snapshots === null
                  ? "Looking for backups…"
                  : snapshots.length === 0
                    ? "There is no backup yet. Install it empty, or back it up first."
                    : undefined
              }
              trailing={
                snapshots && snapshots.length > 0 ? (
                  <Select
                    value={snapshot}
                    onChange={(e) => setSnapshot(e.target.value)}
                    aria-label="Backup to copy from"
                    className="h-9 w-auto max-w-[14rem]"
                  >
                    {snapshots.map((s, i) => (
                      <option key={s.id} value={s.id}>
                        {i === 0 ? "Latest — " : ""}
                        {formatDateTime(s.time)}
                      </option>
                    ))}
                  </Select>
                ) : null
              }
            />
          )}
        </Section>
      )}

      <div className="mt-8">
        <Button
          full
          size="lg"
          onClick={() => void install()}
          disabled={blocker !== null || installing}
        >
          {installing
            ? "Starting…"
            : origin.mode === "restore"
              ? `Restore ${app.name}`
              : `Install ${app.name}`}
        </Button>
        {blocker && (
          <p className="mt-2 text-center text-sm text-fg-muted" role="status">
            {blocker}
          </p>
        )}
      </div>
    </Page>
  );
}
