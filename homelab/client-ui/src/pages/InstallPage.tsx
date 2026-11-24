import { useEffect, useMemo, useRef, useState } from "react";
import {
  Link,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router-dom";
import { ArrowLeft, Download, ExternalLink } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { buttonClass } from "@/components/ui/button-variants";
import { Card } from "@/components/ui/card";
import { Switch } from "@/components/ui/input";
import { Row, Section } from "@/components/ui/list";
import { Banner, Spinner } from "@/components/ui/feedback";
import { api, ApiError, streamEvents } from "@/lib/api";
import { useApi } from "@/lib/useResource";
import { formatDateTime, generateSecret } from "@/lib/format";
import { backupPagePath } from "@/lib/backups";
import Form from "@rjsf/core";
import type { RJSFSchema } from "@rjsf/utils";
import validator from "@rjsf/validator-ajv8";
import { templates, widgets } from "@/components/form/registry";
import {
  changesLiveApp,
  copiesDataByDefault,
  filesFolderCopy,
  installBlocker,
  installOrigin,
  installSource,
  seedForm,
  snapshotNamespace,
  instanceNameFor,
  restoreTargets,
  runningFromBackup,
  type RestoreTarget,
} from "@/lib/install";
import { AppIconTile } from "@/components/AppIcon";
import { RestoreData } from "@/components/RestoreData";
import { NO_CHOICES, choicesFor, type RestoreChoices } from "@/lib/restore";
import { useRestorePlan } from "@/lib/useRestorePlan";
import { useRestorePoints } from "@/lib/useRestorePoints";
import { configSchemaOf, generatedFields, uiSchemaFor } from "@/lib/schema";
import { activeTreeFolders, folderProblem } from "@/lib/treeFolder";
import { taglineFor } from "@/catalog/meta";
import { AppComments, HeartButton } from "@/components/StoreCommunity";
import { AppCard } from "@/components/AppCard";
import { GitHubMark } from "@/components/GitHubMark";
import { PackMembers } from "@/components/PackMembers";
import {
  formatCount,
  githubUrl,
  hasCommunity,
  packMemberEntries,
  similarApps,
  statsById,
  updatedAgo,
  type AppStats,
} from "@/lib/store";
import { cn, webHref } from "@/lib/utils";
import { Swap } from "@/components/motion";
import type {
  AppDefinition,
  AppInfo,
  CatalogApp,
  DomainResponse,
} from "@/types/apps";

function reasonOf(e: unknown, fallback: string): string {
  return e instanceof ApiError ? e.message : fallback;
}

export function InstallPage() {
  const appId = useParams<"appId">().appId!;
  const navigate = useNavigate();

  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const domain = useApi<DomainResponse>("domain", "/api/tunnel/domain");
  const apps = useApi<AppInfo[]>("apps", "/api/apps");
  const stats = useApi<AppStats[]>("store-stats", "/api/store/stats");
  const statsMap = useMemo(() => statsById(stats.data), [stats.data]);

  const catalogApps = catalog.data ?? [];
  const installedApps = apps.data ?? [];
  const cached = catalogApps.find((a) => a.id === appId);

  const [fresh, setFresh] = useState<CatalogApp | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    api
      .post<{ app: CatalogApp | null }>(`/api/apps/catalog/${appId}/refresh`)
      .then(
        (r) => {
          if (!cancelled && r.app) setFresh(r.app);
        },
        (e: unknown) => {
          if (!cancelled)
            setRefreshError(reasonOf(e, "The catalog did not answer."));
        },
      );
    return () => {
      cancelled = true;
    };
  }, [appId]);

  const app = fresh?.id === appId ? fresh : cached;

  useEffect(() => {
    window.scrollTo({ top: 0 });
  }, [appId]);

  const [params, setParams] = useSearchParams();
  const origin = useMemo(() => installOrigin(params), [params]);
  const configuring =
    origin.mode !== "fresh" || params.get("step") === "settings";

  function openSettings() {
    setParams((p) => {
      const next = new URLSearchParams(p);
      next.set("step", "settings");
      return next;
    });
    window.scrollTo({ top: 0 });
  }

  function backToOverview() {
    setParams((p) => {
      const next = new URLSearchParams(p);
      next.delete("step");
      return next;
    });
  }
  const [sourceDef, setSourceDef] = useState<AppDefinition | null>(null);
  const [copyData, setCopyData] = useState(copiesDataByDefault(origin.mode));
  const backupNamespace = snapshotNamespace(origin);
  const { points: snapshots } = useRestorePoints(
    backupNamespace && copyData ? backupNamespace : null,
  );
  const snapshot = origin.mode === "restore" ? origin.snapshot : "";
  const snapshotTime =
    snapshots?.find((s) => s.snapshot_id === snapshot)?.time ?? null;
  const running = runningFromBackup(backupNamespace, installedApps);
  const [target, setTarget] = useState<RestoreTarget | null>(null);
  const where = target ?? restoreTargets(running)[0];
  const replacing =
    origin.mode === "restore" && running !== null && where === "replace";
  const waitingForApps = origin.mode === "restore" && !apps.data && !apps.error;
  const [picked, setPicked] = useState<RestoreChoices>(NO_CHOICES);
  const restorePlan = useRestorePlan(
    origin.mode === "restore" ? origin.namespace : null,
    snapshot,
  );

  const [sourceError, setSourceError] = useState<string | null>(null);
  useEffect(() => {
    if (origin.mode === "fresh") return;
    let cancelled = false;
    const url =
      origin.mode === "restore"
        ? `/api/backups/apps/${origin.namespace}/definition?snapshot_id=${encodeURIComponent(origin.snapshot)}`
        : `/api/apps/${origin.fromInstance}/definition`;
    api.get<AppDefinition>(url).then(
      (d) => {
        if (!cancelled) setSourceDef(d);
      },
      (e: unknown) => {
        if (!cancelled)
          setSourceError(
            `Its settings could not be read: ${reasonOf(e, "the server did not answer")}`,
          );
      },
    );
    return () => {
      cancelled = true;
    };
  }, [origin]);

  const [liveSchema, setLiveSchema] = useState<object | null>(null);
  useEffect(() => {
    if (origin.mode !== "edit") return;
    let cancelled = false;
    api.get<object>(`/api/apps/${origin.fromInstance}/schema`).then(
      (s) => {
        if (!cancelled) setLiveSchema(s);
      },
      (e: unknown) => {
        if (!cancelled)
          setSourceError(
            `Its settings form could not be read: ${reasonOf(e, "the server did not answer")}`,
          );
      },
    );
    return () => {
      cancelled = true;
    };
  }, [origin]);

  const schema = useMemo(
    () => configSchemaOf(liveSchema ?? app?.schema),
    [liveSchema, app?.schema],
  );
  const required = useMemo(
    () => new Set(schema.required ?? []),
    [schema.required],
  );

  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [started, setStarted] = useState<string | null>(null);

  const installedOfThisApp = installedApps.filter((a) => a.app_id === appId);
  const instanceName = instanceNameFor(origin.mode, appId, sourceDef);

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
    if (!schema.properties || waitingForApps) return;
    const key = `${appId}|${sourceDef ? "source" : "new"}|${running ?? ""}`;
    if (seeded.current === key) return;
    seeded.current = key;
    const seed = seedForm(
      schema,
      sourceDef ? (sourceDef.config as Record<string, unknown>) : null,
      origin.mode,
      generateSecret,
      running !== null,
    );
    setFormData(seed);
  }, [appId, schema, sourceDef, origin.mode, waitingForApps, running]);

  const values = formData;
  const filesCopy =
    origin.mode === "duplicate"
      ? filesFolderCopy(
          sourceDef ? (sourceDef.config as Record<string, unknown>) : null,
          values,
        )
      : null;

  const generatedSecrets = useMemo(() => generatedFields(schema), [schema]);

  const folderKey = JSON.stringify(activeTreeFolders(schema, values));
  const [folderCheck, setFolderCheck] = useState<{
    key: string;
    problem: string | null;
  } | null>(null);
  useEffect(() => {
    let live = true;
    const fields = JSON.parse(folderKey) as { title: string; value: string }[];
    folderProblem(fields).then((problem) => {
      if (live) setFolderCheck({ key: folderKey, problem });
    });
    return () => {
      live = false;
    };
  }, [folderKey]);
  const folderBlock =
    folderKey === "[]"
      ? null
      : folderCheck?.key === folderKey
        ? folderCheck.problem
        : "Checking the folder in your file tree…";

  const formBlocker = installBlocker({
    instanceName,
    requiredMissing: [...required].some((n) => !String(values[n] ?? "").trim()),
    folderProblem: folderBlock,
    withData: copyData,
    needsBackup: origin.mode === "restore",
    snapshot,
    snapshotsLoaded: true,
    snapshotCount: snapshot ? 1 : 0,
  });
  const blocker = sourceError
    ? sourceError
    : replacing
      ? restorePlan.plan
        ? null
        : (restorePlan.error ?? "Reading the backup…")
      : formBlocker;

  async function install(app: CatalogApp) {
    setInstalling(true);
    setError(null);

    if (replacing) {
      try {
        await api.post("/api/backups/restore", {
          namespace: origin.namespace,
          snapshot_id: snapshot,
          choices: choicesFor(restorePlan.plan!, "in-place", picked),
        });
        navigate(`/app/${running}`);
      } catch (e) {
        setError(reasonOf(e, "The restore did not start."));
        setInstalling(false);
      }
      return;
    }

    const payload = Object.fromEntries(
      Object.entries(values).filter(([name, v]) => {
        if (v !== "") return true;
        return schema.properties?.[name]?.default !== undefined;
      }),
    );

    if (changesLiveApp(origin.mode)) {
      const result = await streamEvents(
        `/api/apps/${instanceName}/update`,
        {
          method: "POST",
          body: JSON.stringify({
            config: payload,
            keep_version: origin.mode === "edit",
          }),
        },
        () => {},
      );
      if (result.ok) {
        navigate(`/app/${instanceName}`);
      } else {
        setError(result.error);
        setInstalling(false);
      }
      return;
    }

    try {
      const begun = await api.post<{ instance_name: string }>(
        `/api/apps/${app.id}`,
        {
          instance_name: instanceName,
          config: payload,
          source: installSource(
            origin,
            copyData,
            origin.mode === "restore" && restorePlan.plan
              ? choicesFor(restorePlan.plan, "reinstall", picked)
              : undefined,
          ),
        },
      );
      if (generatedSecrets.length > 0) setStarted(begun.instance_name);
      else navigate(`/app/${begun.instance_name}`);
    } catch (e) {
      setError(reasonOf(e, "The install could not start."));
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
      <Page title="Service not found">
        <p className="text-sm text-fg-muted">
          {refreshError ?? "That service is not in the catalog any more."}
        </p>
        <Link
          to="/add"
          className={cn(buttonClass({ variant: "secondary" }), "mt-4")}
        >
          Back to services
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
                    {String(values[name])}
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

  const shownName =
    sourceDef?.instance_name ??
    (origin.mode === "duplicate"
      ? origin.fromInstance
      : origin.mode === "restore"
        ? origin.namespace
        : app.name);

  const notice = error ? (
    <Banner
      tone="error"
      title={
        origin.mode === "update"
          ? "The update did not finish"
          : origin.mode === "edit"
            ? "The new settings were not applied"
            : "The install could not start"
      }
    >
      {error}
    </Banner>
  ) : app.repo !== "official" ? (
    <Banner tone="warning" title={`This service comes from "${app.repo}"`}>
      You added this source yourself. Services from outside the official catalog
      can do anything on your machines, so only install ones you trust.
    </Banner>
  ) : null;

  const backLink =
    "mb-6 inline-flex items-center gap-1.5 rounded-control text-sm text-fg-muted hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary";

  if (!configuring) {
    const tagline = taglineFor(app);
    const appStats = statsMap.get(app.id);
    const installs = appStats?.installs ?? 0;
    const repo = githubUrl(app);
    const home = webHref(app.home);
    const website = home && home !== repo ? home : null;
    const updated = app.pushed_at ? updatedAgo(app.pushed_at) : null;
    const explains = app.description && app.description !== tagline;
    const similar = similarApps(app, catalogApps, statsMap);
    const factLink =
      "inline-flex items-center gap-1.5 rounded-control transition-colors hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary";

    return (
      <Page wide>
        <Link to="/add" className={backLink}>
          <ArrowLeft className="h-4 w-4" />
          All services
        </Link>

        <Swap id="overview" className="block">
          <header className="grid gap-6 sm:grid-cols-[1fr_auto] sm:items-end">
            <div className="min-w-0">
              <AppIconTile
                appId={app.id}
                icon={app.icon}
                name={app.name}
                size="lg"
                className="mb-6"
              />
              <h1 className="font-display text-[2.4rem] font-semibold leading-[1.05] tracking-[-0.025em] text-fg md:text-[3.25rem]">
                {app.name}
              </h1>
              <p className="mt-3 max-w-[40ch] text-lg leading-snug text-fg-muted">
                {tagline}
              </p>
            </div>
            <div className="flex flex-col gap-2 sm:items-end">
              <div className="flex items-start gap-2">
                <Button size="lg" onClick={openSettings}>
                  Install {app.name}
                </Button>
                <HeartButton
                  key={app.id}
                  app={app}
                  count={appStats?.hearts ?? 0}
                />
              </div>
              {installedOfThisApp.length > 0 && (
                <p className="text-xs text-fg-subtle">
                  Installing again adds a separate copy
                </p>
              )}
            </div>
          </header>

          <ul className="mt-8 flex flex-wrap items-center gap-x-6 gap-y-3 border-y border-border py-4 text-sm text-fg-muted">
            {hasCommunity(app) && (
              <li
                className="inline-flex items-center gap-1.5"
                title="YoLab accounts that installed it"
              >
                <Download className="h-4 w-4" aria-hidden />
                <span className="font-mono tabular-nums text-fg">
                  {formatCount(installs)}
                </span>
                {installs === 1 ? "install" : "installs"}
              </li>
            )}
            {repo && (
              <li>
                <a
                  href={repo}
                  target="_blank"
                  rel="noreferrer noopener"
                  className={factLink}
                  title={`${app.github} on GitHub`}
                >
                  <GitHubMark className="h-3.5 w-3.5" />
                  {app.stars !== null && app.stars > 0 ? (
                    <>
                      <span className="font-mono tabular-nums text-fg">
                        {formatCount(app.stars)}
                      </span>
                      stars
                    </>
                  ) : (
                    "Source code"
                  )}
                </a>
              </li>
            )}
            {updated && <li>Updated {updated}</li>}
            {website && (
              <li className="sm:ml-auto">
                <a
                  href={website}
                  target="_blank"
                  rel="noreferrer noopener"
                  className={factLink}
                >
                  Project website
                  <ExternalLink className="h-3.5 w-3.5" aria-hidden />
                </a>
              </li>
            )}
          </ul>
        </Swap>

        {notice && <div className="mt-8">{notice}</div>}

        <div className="max-w-3xl">
          {explains && (
            <section className="mt-10">
              <h2 className="mb-2 px-1 text-sm font-semibold text-fg-muted">
                What it does
              </h2>
              <p className="max-w-[62ch] px-1 leading-relaxed text-fg">
                {app.description}
              </p>
            </section>
          )}

          <PackMembers members={packMemberEntries(app, catalogApps)} />

          {installedOfThisApp.length > 0 && (
            <Section title="You already run">
              {installedOfThisApp.map((a) => (
                <Row
                  key={a.instance_name}
                  label={a.instance_name}
                  detail={a.detail}
                  onClick={() => navigate(`/app/${a.instance_name}`)}
                />
              ))}
            </Section>
          )}

          <AppComments app={app} />
        </div>

        {similar.length > 0 && (
          <section className="mt-14 border-t border-border pt-8">
            <h2 className="font-display text-xl font-semibold tracking-tight text-fg">
              Similar services
            </h2>
            <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {similar.map((other) => (
                <AppCard
                  key={`${other.repo}/${other.id}`}
                  app={other}
                  count={
                    installedApps.filter((a) => a.app_id === other.id).length
                  }
                  stats={statsMap.get(other.id)}
                />
              ))}
            </div>
          </section>
        )}
      </Page>
    );
  }

  const heading =
    origin.mode === "duplicate"
      ? `Duplicate ${shownName}`
      : origin.mode === "restore"
        ? `Restore ${shownName}`
        : origin.mode === "edit"
          ? `Settings of ${shownName}`
          : origin.mode === "update"
            ? `Update ${shownName} to ${app.chart_version}`
            : `Install ${app.name}`;

  const restoreLabel = replacing
    ? `Replace ${running} with this copy`
    : `Add ${app.name} from this copy`;

  const subtitle =
    origin.mode === "duplicate"
      ? "A separate service from the same settings, with its own address and storage."
      : replacing
        ? `${running} goes back to how it was in this copy, settings included.`
        : origin.mode === "restore"
          ? "A new service from this copy. Its settings come back from the copy; change any of them before it is added."
          : origin.mode === "edit"
            ? "Applied to the version it runs now. Its files stay as they are."
            : origin.mode === "update"
              ? "The settings of the new version, filled in with the ones it has now. Check them, add anything new it asks for, then update. Its files stay as they are."
              : "Choose how it is set up. You can change these settings later.";

  return (
    <Page>
      {origin.mode === "fresh" ? (
        <button type="button" onClick={backToOverview} className={backLink}>
          <ArrowLeft className="h-4 w-4" />
          {app.name}
        </button>
      ) : origin.mode === "restore" ? (
        <Link to={backupPagePath(origin.namespace)} className={backLink}>
          <ArrowLeft className="h-4 w-4" />
          Its backups
        </Link>
      ) : origin.mode === "duplicate" ? (
        <Link to="/add" className={backLink}>
          <ArrowLeft className="h-4 w-4" />
          All services
        </Link>
      ) : (
        <Link to={`/app/${origin.fromInstance}`} className={backLink}>
          <ArrowLeft className="h-4 w-4" />
          {shownName}
        </Link>
      )}

      <Swap id="settings" className="block">
        <header className="flex items-center gap-4">
          <AppIconTile appId={app.id} icon={app.icon} name={app.name} />
          <div className="min-w-0 flex-1">
            <h1 className="font-display text-[1.75rem] leading-tight text-fg md:text-4xl">
              {heading}
            </h1>
            <p className="mt-1 text-sm text-fg-muted">{subtitle}</p>
          </div>
        </header>
      </Swap>

      {notice && <div className="mt-6">{notice}</div>}

      {origin.mode === "restore" && running && (
        <Section title="Where it goes">
          {restoreTargets(running).map((t) => (
            <label
              key={t}
              className="flex cursor-pointer items-start gap-3 px-4 py-3 hover:bg-surface-2"
            >
              <input
                type="radio"
                name="restore-target"
                checked={where === t}
                onChange={() => setTarget(t)}
                className="mt-1 accent-primary"
              />
              <span className="min-w-0">
                <span className="block text-sm font-medium text-fg">
                  {t === "replace" ? `Replace ${running}` : "As a new service"}
                </span>
                <span className="block text-sm text-fg-muted">
                  {t === "replace"
                    ? "Same name and address, so phones and bookmarks keep working. What it has now is replaced by this copy."
                    : `Added next to ${running}, with its own name, address and folder. ${running} stays as it is.`}
                </span>
              </span>
            </label>
          ))}
        </Section>
      )}

      {!replacing && (
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
              onChange={(e) =>
                setFormData(e.formData as Record<string, unknown>)
              }
            >
              <></>
            </Form>
          </Card>
        </section>
      )}

      {origin.mode === "duplicate" && (
        <Section title="Files">
          <Row
            label="Copy this service’s files too"
            detail={
              copyData
                ? "Copied straight from the service, as they are now."
                : "It starts empty."
            }
            trailing={
              <Switch
                checked={copyData}
                onChange={setCopyData}
                label="Copy this service’s files too"
              />
            }
          />
          {copyData && filesCopy?.kind === "shared" && (
            <Row
              label="Files folder"
              detail={`It shares /${filesCopy.to} with the original, so nothing is copied there.`}
            />
          )}
          {copyData && filesCopy?.kind === "existing" && (
            <Row
              label="Files folder"
              detail={`The files are copied into /${filesCopy.to}. If it already has files in it, it is used as it is.`}
            />
          )}
        </Section>
      )}

      {origin.mode === "restore" && (
        <Section title={replacing ? "What comes back" : "Files"}>
          <Row
            label="From the copy of"
            detail={snapshotTime ? formatDateTime(snapshotTime) : snapshot}
            trailing={
              <Link
                to={backupPagePath(origin.namespace)}
                className="text-sm text-primary"
              >
                Pick another moment
              </Link>
            }
          />
          {snapshot && (
            <RestoreData
              plan={restorePlan.plan}
              error={restorePlan.error}
              mode={replacing ? "in-place" : "reinstall"}
              app={replacing ? running : shownName}
              picked={picked}
              onPick={setPicked}
            />
          )}
        </Section>
      )}

      <div className="mt-8">
        <Button
          full
          size="lg"
          onClick={() => void install(app)}
          disabled={blocker !== null || installing}
        >
          {installing
            ? origin.mode === "update"
              ? "Updating…"
              : origin.mode === "edit"
                ? "Applying…"
                : "Starting…"
            : origin.mode === "restore"
              ? restoreLabel
              : origin.mode === "edit"
                ? "Apply these settings"
                : origin.mode === "update"
                  ? `Update to ${app.chart_version}`
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
