import { AnimatedList, Collapse, Swap } from "@/components/motion";
import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowLeft, RefreshCw } from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppAccess } from "@/components/AppAccess";
import { AppIconTile } from "@/components/AppIcon";
import { useNow } from "@/lib/useNow";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/sheet";
import { RemoveDataChoice } from "@/components/RemoveDataChoice";
import { removePath } from "@/lib/removeData";
import {
  Banner,
  ServiceTrouble,
  Skeleton,
  Spinner,
} from "@/components/ui/feedback";
import {
  DisclosureRow,
  Row,
  RowAction,
  Section,
  ValueRow,
} from "@/components/ui/list";
import { api, ApiError, streamEvents } from "@/lib/api";
import { backupPagePath, scheduleLabel } from "@/lib/backups";
import { formatDateTime, relativeTime } from "@/lib/format";
import { useApi } from "@/lib/useResource";
import {
  appDisplayName,
  appLinks,
  appState,
  appStatus,
  availableActions,
  catalogEntry,
  instanceStem,
  latestRestore,
  newerVersion,
  packMembers,
  podProblem,
  podsByMember,
  podStatus,
  waitNote,
  type AppState,
  type RestoreRecord,
  type StatusTone,
} from "@/lib/apps";
import { taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp, PodInfo } from "@/types/apps";

function reasonOf(e: unknown, fallback: string): string {
  return e instanceof ApiError ? e.message : fallback;
}

const DOT: Record<StatusTone, string> = {
  live: "bg-success",
  busy: "bg-primary animate-pulse",
  warn: "bg-warning",
  error: "bg-danger",
};

function StatusLine({
  state,
  version,
  reason,
}: {
  state: AppState;
  version: string;
  reason?: string;
}) {
  const { tone, label } = appStatus(state, reason);
  return (
    <p className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-sm text-fg-muted">
      <span className="inline-flex items-center gap-1.5">
        <span
          className={cn(
            "h-2 w-2 rounded-full transition-colors duration-300",
            DOT[tone],
          )}
          aria-hidden
        />
        <Swap id={label}>{label}</Swap>
      </span>
      {version && (
        <>
          <span aria-hidden className="text-fg-subtle">
            ·
          </span>
          <span className="font-mono text-xs tabular-nums">v{version}</span>
        </>
      )}
    </p>
  );
}

function WhatHappened({ app, fallback }: { app: AppInfo; fallback: string }) {
  const said = app.technical?.trim();
  return (
    <>
      <p>{app.detail?.trim() || fallback}</p>
      {said && (
        <details className="my-2">
          <summary className="cursor-pointer text-xs text-fg-muted hover:text-fg">
            What the system said
          </summary>
          <pre className="mt-1.5 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded-control bg-surface p-2.5 font-mono text-xs text-fg">
            {said}
          </pre>
        </details>
      )}
    </>
  );
}

function WaitNote({ note }: { note: string | null }) {
  if (!note) return null;
  return <p className="mt-1 tabular-nums">{note}</p>;
}

const POD_REFRESH_MS = 5000;
const MAX_LOG_LINES = 1000;

function TechnicalDetails({ app }: { app: AppInfo }) {
  const [pods, setPods] = useState<PodInfo[] | null>(null);
  const [podsError, setPodsError] = useState<string | null>(null);
  const [logs, setLogs] = useState<{
    pod: string;
    lines: string[];
    live: boolean;
  } | null>(null);
  const logStream = useRef<AbortController | null>(null);
  const logBox = useRef<HTMLPreElement | null>(null);

  function stopLogs() {
    logStream.current?.abort();
    logStream.current = null;
    setLogs((l) => ({ ...l!, live: false }));
  }

  function startLogs(pod: string) {
    logStream.current?.abort();
    const ctrl = new AbortController();
    logStream.current = ctrl;
    setLogs({ pod, lines: [], live: true });
    const settle = () => {
      if (logStream.current === ctrl) {
        setLogs((l) => ({ ...l!, live: false }));
      }
    };
    void streamEvents(
      `/api/apps/${app.instance_name}/logs/${pod}`,
      { signal: ctrl.signal },
      (line) => {
        setLogs((prev) =>
          prev && prev.pod === pod
            ? { ...prev, lines: [...prev.lines, line].slice(-MAX_LOG_LINES) }
            : prev,
        );
      },
    ).then(settle);
  }

  useEffect(() => () => logStream.current?.abort(), []);

  useEffect(() => {
    const el = logBox.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [logs?.lines.length]);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      api.get<PodInfo[]>(`/api/apps/${app.instance_name}/pods`).then(
        (p) => {
          if (cancelled) return;
          setPods(p);
          setPodsError(null);
        },
        (e: unknown) => {
          if (!cancelled)
            setPodsError(reasonOf(e, "The server did not answer."));
        },
      );
    load();
    const timer = setInterval(load, POD_REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [app.instance_name]);

  const settings = Object.entries(app.config);

  return (
    <>
      <div>
        <h3 className="mb-1.5 text-xs font-medium text-fg-muted">
          Running parts
        </h3>
        {podsError && (
          <p className="mb-1.5 text-sm text-danger" role="alert">
            The running parts could not be read: {podsError}
          </p>
        )}
        {!pods ? (
          podsError ? null : (
            <Spinner className="h-4 w-4" />
          )
        ) : pods.length === 0 ? (
          <p className="text-sm text-fg-muted">Nothing running right now.</p>
        ) : (
          podsByMember(pods).map((section) => (
            <div key={section.member ?? ""} className="mb-2 last:mb-0">
              {section.member && (
                <h4 className="mb-1 text-xs font-medium text-fg">
                  {section.member}
                </h4>
              )}
              <ul className="divide-y divide-border rounded-control border border-border">
                <AnimatedList
                  as="li"
                  items={section.pods}
                  keyOf={(pod) => pod.name}
                  itemClassName="px-3 py-1.5"
                >
                  {(pod) => {
                    const status = podProblem(pod) ?? podStatus(pod);
                    const containers = pod.containers ?? [];
                    const events = pod.events ?? [];
                    return (
                      <>
                        <div className="flex items-center gap-2">
                          <span
                            className={cn(
                              "h-2 w-2 shrink-0 rounded-full transition-colors duration-300",
                              pod.ready ? "bg-success" : "bg-warning",
                            )}
                            aria-hidden
                          />
                          <span className="min-w-0 flex-1 truncate font-mono text-xs text-fg-muted">
                            {pod.name}
                          </span>
                          <span
                            className="shrink-0 text-xs text-fg-subtle"
                            title={pod.phase}
                          >
                            <Swap id={status}>{status}</Swap>
                          </span>
                          <RowAction
                            onClick={() =>
                              logs?.pod === pod.name && logs.live
                                ? stopLogs()
                                : startLogs(pod.name)
                            }
                          >
                            {logs?.pod === pod.name && logs.live
                              ? "Stop"
                              : "Logs"}
                          </RowAction>
                        </div>
                        <p className="mt-0.5 pl-4 text-xs text-fg-subtle">
                          {pod.node
                            ? `On ${pod.node}`
                            : "Not placed on a machine yet"}
                          {pod.restarts ? ` · restarted ${pod.restarts}×` : ""}
                        </p>
                        {containers.length > 0 && (
                          <ul className="mt-1 space-y-0.5 pl-4">
                            {containers.map((c) => (
                              <li
                                key={`${c.init ? "init-" : ""}${c.name}`}
                                className="flex items-center gap-2 text-xs"
                              >
                                <span
                                  className={cn(
                                    "h-1.5 w-1.5 shrink-0 rounded-full",
                                    c.ready || c.state === "Completed"
                                      ? "bg-success"
                                      : c.state === "running" ||
                                          c.state === "PodInitializing"
                                        ? "bg-warning"
                                        : "bg-danger",
                                  )}
                                  aria-hidden
                                />
                                <span className="min-w-0 flex-1 truncate font-mono text-fg-muted">
                                  {c.name}
                                  {c.init && (
                                    <span className="text-fg-subtle">
                                      {" "}
                                      (setup)
                                    </span>
                                  )}
                                </span>
                                <span className="shrink-0 text-fg-subtle">
                                  {c.state}
                                  {c.restarts > 0 ? ` · ${c.restarts}×` : ""}
                                </span>
                              </li>
                            ))}
                          </ul>
                        )}
                        {events.length > 0 && (
                          <ul className="mt-1.5 space-y-0.5 border-l border-border pl-3 ml-4">
                            {events.map((e, i) => (
                              <li
                                key={`${e.at}-${e.reason}-${i}`}
                                className={cn(
                                  "text-xs",
                                  e.warning ? "text-warning" : "text-fg-subtle",
                                )}
                              >
                                <span title={formatDateTime(e.at)}>
                                  {relativeTime(e.at)}
                                </span>
                                {" · "}
                                {e.message || e.reason}
                                {e.count > 1 ? ` (${e.count}×)` : ""}
                              </li>
                            ))}
                          </ul>
                        )}
                      </>
                    );
                  }}
                </AnimatedList>
              </ul>
            </div>
          ))
        )}
      </div>

      {logs && (
        <div>
          <p className="mb-1.5 flex items-center gap-2 text-xs text-fg-muted">
            <span className="truncate font-mono">{logs.pod}</span>
            {logs.live ? (
              <span className="inline-flex items-center gap-1 text-success">
                <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-success" />
                Live
              </span>
            ) : (
              <span>Stopped</span>
            )}
          </p>
          <pre
            ref={logBox}
            className="max-h-72 overflow-auto rounded-control bg-surface-2 p-3 font-mono text-xs leading-relaxed text-fg-muted"
          >
            {logs.lines.length > 0
              ? logs.lines.join("\n")
              : logs.live
                ? "Connected — waiting for this service to print something…"
                : "This service printed nothing."}
          </pre>
        </div>
      )}

      {settings.length > 0 && (
        <div>
          <h3 className="mb-1.5 text-xs font-medium text-fg-muted">
            Installed with
          </h3>
          <dl className="space-y-1 rounded-control bg-surface-2 p-3">
            <AnimatedList
              items={settings}
              keyOf={([k]) => k}
              itemClassName="flex gap-3 text-xs"
            >
              {([k, v]) => {
                const text = typeof v === "string" ? v : JSON.stringify(v);
                return (
                  <>
                    <dt className="shrink-0 text-fg-subtle">{k}</dt>
                    <dd className="min-w-0 flex-1 break-all font-mono text-fg-muted">
                      <Swap id={text} className="inline">
                        {text}
                      </Swap>
                    </dd>
                  </>
                );
              }}
            </AnimatedList>
          </dl>
        </div>
      )}
    </>
  );
}

function BackupsSection({ app, canOpen }: { app: AppInfo; canOpen: boolean }) {
  const navigate = useNavigate();
  const last = app.backup.last_ok_at;
  const detail = !app.backup.enabled
    ? "Off"
    : app.backup.running
      ? "Backing up now…"
      : [
          scheduleLabel(app.backup.schedule),
          last ? `last saved ${relativeTime(last)}` : "not saved yet",
        ].join(" · ");

  return (
    <Section title="Backups">
      <Row
        label="Its backups"
        detail={detail}
        onClick={
          canOpen
            ? () => navigate(backupPagePath(`yolab-${app.instance_name}`))
            : undefined
        }
      />
    </Section>
  );
}

type Notice =
  | { tone: "success"; title: string; body?: string }
  | { tone: "error"; title: string; body: string };

export function AppDetailPage() {
  const instanceName = useParams<"instanceName">().instanceName!;
  const navigate = useNavigate();

  const apps = useApi<AppInfo[]>("apps", "/api/apps", { pollMs: 10_000 });
  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");

  const [confirmRemove, setConfirmRemove] = useState(false);
  const [deleteShared, setDeleteShared] = useState(false);
  const [deleteBackups, setDeleteBackups] = useState(false);
  const [working, setWorking] = useState<null | "update" | "remove">(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [restore, setRestore] = useState<RestoreRecord | null>(null);
  const [restoreSeen, setRestoreSeen] = useState<string | null>(null);
  const [restoreError, setRestoreError] = useState<string | null>(null);

  const installed = apps.data ?? [];
  const catalogApps = catalog.data ?? [];
  const app = installed.find((a) => a.instance_name === instanceName);
  const state = app ? appState(app) : "starting";
  const now = useNow(5_000);

  useEffect(() => {
    let cancelled = false;
    function poll() {
      api.get<RestoreRecord[]>("/api/backups/restores").then(
        (list) => {
          if (cancelled) return;
          setRestore(latestRestore(list, `yolab-${instanceName}`, Date.now()));
          setRestoreError(null);
        },
        (e: unknown) => {
          if (!cancelled)
            setRestoreError(reasonOf(e, "The server did not answer."));
        },
      );
    }
    poll();
    const id = window.setInterval(poll, 5000);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [instanceName]);

  if (apps.loading) {
    return (
      <Page>
        <Skeleton className="h-32 w-full" />
      </Page>
    );
  }

  if (apps.error && !apps.data) {
    return (
      <Page>
        <ServiceTrouble onRetry={apps.refresh} />
      </Page>
    );
  }

  if (!app) {
    return (
      <Page title="Service not found">
        <p className="text-sm text-fg-muted">
          There is no service called “{instanceName}” on this server. It may
          have been removed.
        </p>
        <Link
          to="/"
          className="mt-4 inline-flex items-center gap-1.5 text-sm text-primary"
        >
          <ArrowLeft className="h-4 w-4" />
          Back to my services
        </Link>
      </Page>
    );
  }

  const entry = catalogEntry(app, catalogApps);
  const name = appDisplayName(app, catalogApps, installed);
  const members = packMembers(app, catalogApps);
  const links = appLinks(app);
  const restoring = restore?.state === "running";
  const actions = availableActions(state, restoring);
  const newer = newerVersion(app, entry);
  const offeredUpdate = newer && actions.has("update") ? newer : null;
  const version = app.chart_version || entry?.chart_version || "";
  const stem = instanceStem(app);
  const current = app;

  async function remove() {
    setWorking("remove");
    setNotice(null);
    try {
      await api.del(
        removePath(current.instance_name, deleteShared, deleteBackups),
      );
      navigate("/");
    } catch (e) {
      setNotice({
        tone: "error",
        title: "It could not be removed",
        body: reasonOf(e, "The server did not accept that."),
      });
      setWorking(null);
    }
  }

  async function update() {
    setWorking("update");
    setNotice(null);
    let reached = "";
    const result = await streamEvents(
      `/api/apps/${current.instance_name}/update`,
      { method: "POST" },
      (line) => {
        const found = /^Now on version (.+)$/.exec(line);
        if (found) reached = found[1];
      },
    );
    if (result.ok) {
      setNotice({
        tone: "success",
        title: reached
          ? `${name} now runs version ${reached}`
          : `${name} was reinstalled`,
      });
    } else {
      setNotice({
        tone: "error",
        title: "The update did not finish",
        body: result.error,
      });
    }
    await Promise.all([apps.refresh(), catalog.refresh()]);
    setWorking(null);
  }

  const restoreDone =
    restore && restore.state !== "running" && restore.id !== restoreSeen
      ? restore
      : null;
  const note = waitNote(current, state, now);

  function banner() {
    if (state === "removing") {
      return (
        <Banner tone="warning" title="Being removed">
          <WhatHappened
            app={current}
            fallback="Your request was saved and it is being removed in the background. You can leave this page."
          />
          <WaitNote note={note} />
        </Banner>
      );
    }
    if (state === "failed") {
      return (
        <Banner
          tone="error"
          title="The installation failed"
          action={
            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                variant="secondary"
                onClick={() => void update()}
                loading={working === "update"}
              >
                <RefreshCw className="h-4 w-4" />
                Try again
              </Button>
              <Button
                size="sm"
                variant="secondary"
                onClick={() =>
                  navigate(
                    `/add/${current.app_id}?edit=${current.instance_name}`,
                  )
                }
              >
                Change settings
              </Button>
              <Button
                size="sm"
                variant="secondary"
                onClick={() => setConfirmRemove(true)}
              >
                Remove it
              </Button>
            </div>
          }
        >
          <WhatHappened app={current} fallback="It did not say why." />
          {notice?.tone === "error" && (
            <p className="mt-1 font-medium">
              {notice.title}: {notice.body}
            </p>
          )}
          {note ? (
            <WaitNote note={`It keeps trying on its own. ${note}`} />
          ) : (
            <p className="mt-1">It is kept so you can see what went wrong.</p>
          )}
        </Banner>
      );
    }
    if (restoring) {
      return (
        <Banner tone="info" title={`Restoring ${name}`}>
          It is offline while its files and settings are brought back, and comes
          back on its own when that finishes.
          {restoreError && (
            <p className="mt-1">
              Its progress could not be read just now: {restoreError}
            </p>
          )}
        </Banner>
      );
    }
    if (notice?.tone === "error") {
      return (
        <Banner tone="error" title={notice.title}>
          {notice.body}
        </Banner>
      );
    }
    if (restoreDone?.state === "failed") {
      return (
        <Banner
          tone="error"
          title="The restore did not finish"
          action={
            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                variant="secondary"
                onClick={() =>
                  navigate(backupPagePath(`yolab-${current.instance_name}`))
                }
              >
                Pick a backup again
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => setRestoreSeen(restoreDone.id)}
              >
                Dismiss
              </Button>
            </div>
          }
        >
          {restoreDone.error ?? "Something went wrong while restoring it."}
        </Banner>
      );
    }
    if (state === "stopped") {
      return (
        <Banner tone="warning" title="It stopped working">
          <WhatHappened app={current} fallback="It did not say why." />
          <WaitNote
            note={`It keeps trying to start again on its own.${note ? ` ${note}` : ""}`}
          />
        </Banner>
      );
    }
    if (restoreDone?.state === "succeeded") {
      return (
        <Banner
          tone="success"
          title={`${name} is restored`}
          action={
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setRestoreSeen(restoreDone.id)}
            >
              Dismiss
            </Button>
          }
        >
          Its files and settings are back from the backup
          {`, finished ${relativeTime(restoreDone.finished_at!)}`}.
        </Banner>
      );
    }
    if (notice?.tone === "success") {
      return (
        <Banner
          tone="success"
          title={notice.title}
          action={
            <Button size="sm" variant="ghost" onClick={() => setNotice(null)}>
              Dismiss
            </Button>
          }
        />
      );
    }
    if (state === "copying") {
      return (
        <Banner tone="info" title="Copying its files">
          {current.detail?.trim() ||
            "It starts on its own once its files are in place."}
          <WaitNote note={note} />
        </Banner>
      );
    }
    if (state === "starting") {
      return (
        <Banner tone="info" title="Starting up">
          <WhatHappened
            app={current}
            fallback="This usually takes a minute or two the first time."
          />
          <WaitNote note={note} />
        </Banner>
      );
    }
    return null;
  }

  const shown = banner();
  const quiet = state === "failed" || state === "removing";

  return (
    <Page>
      <Link
        to="/"
        className="mb-5 inline-flex items-center gap-1.5 rounded-control text-sm text-fg-muted hover:text-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary"
      >
        <ArrowLeft className="h-4 w-4" />
        My services
      </Link>

      <header className="flex flex-wrap items-center gap-4">
        <AppIconTile appId={app.app_id} icon={entry?.icon} name={name} />
        <div className="min-w-0 flex-1">
          <h1 className="font-display text-[1.75rem] leading-tight text-fg md:text-4xl">
            {name}
          </h1>
          <StatusLine state={state} version={version} reason={app.reason} />
        </div>
      </header>

      <Collapse open={Boolean(shown)} className="pt-6">
        {shown}
      </Collapse>

      {!quiet && <AppAccess instanceName={app.instance_name} links={links} />}

      {!quiet && (
        <BackupsSection
          app={app}
          canOpen={actions.has("backup") || actions.has("restore")}
        />
      )}

      <Section title="About">
        {!quiet && (
          <Row
            label="What it is"
            detail={
              entry
                ? taglineFor(entry)
                : "Installed from a chart that is no longer in the catalog."
            }
          />
        )}
        <Row
          label="Version"
          detail={
            <span className="font-mono tabular-nums">
              {version || "unknown"}
              {offeredUpdate && (
                <span className="font-sans text-fg-subtle">
                  {" "}
                  · {offeredUpdate} available
                </span>
              )}
            </span>
          }
          trailing={
            offeredUpdate ? (
              <RowAction
                onClick={() =>
                  navigate(`/add/${app.app_id}?update=${app.instance_name}`)
                }
              >
                {`Update to ${offeredUpdate}`}
              </RowAction>
            ) : null
          }
        />
        {entry && entry.repo !== "official" && (
          <Row
            label="Comes from"
            detail={`"${entry.repo}", a source you added yourself`}
          />
        )}
        {members.length > 0 && (
          <Row
            label="Services inside"
            detail={`${members.join(", ")}, updated and removed together`}
          />
        )}
        {stem !== app.app_id && <Row label="Name" detail={stem} />}
        {app.instance_id && (
          <ValueRow label="ID" value={app.instance_id} copy />
        )}
        {actions.has("settings") && (
          <Row
            label="Change settings"
            detail="Change what it was installed with"
            onClick={() =>
              navigate(`/add/${app.app_id}?edit=${app.instance_name}`)
            }
          />
        )}
        {actions.has("duplicate") && (
          <Row
            label="Duplicate"
            detail="A separate copy with its own address and storage"
            onClick={() =>
              navigate(`/add/${app.app_id}?from=${app.instance_name}`)
            }
          />
        )}
        <DisclosureRow
          label="Technical details"
          detail="Running parts, logs and the settings it was installed with"
        >
          <TechnicalDetails app={app} />
        </DisclosureRow>
      </Section>

      {actions.has("remove") && state !== "failed" && (
        <Section title="Remove">
          <Row
            label={`Remove ${name}`}
            detail="A space no other service uses is deleted with it"
            danger
            onClick={() => setConfirmRemove(true)}
          />
        </Section>
      )}

      <ConfirmDialog
        open={confirmRemove}
        onClose={() => setConfirmRemove(false)}
        onConfirm={() => void remove()}
        title={`Remove ${name}?`}
        destructive
        confirmLabel="Remove it"
        busy={working === "remove"}
        body={
          <>
            {name} stops and is removed.{" "}
            {deleteBackups
              ? "Its backups are deleted too."
              : "Its backups are kept, so you can add it back later."}
            <RemoveDataChoice
              instance={app.instance_name}
              open={confirmRemove}
              deleteShared={deleteShared}
              onChange={setDeleteShared}
            />
            <label className="mt-4 flex cursor-pointer items-center gap-2">
              <input
                type="checkbox"
                checked={deleteBackups}
                onChange={(e) => setDeleteBackups(e.target.checked)}
                className="h-4 w-4 accent-primary"
              />
              <span className="text-sm text-fg">Also delete its backups</span>
            </label>
          </>
        }
      />
    </Page>
  );
}
