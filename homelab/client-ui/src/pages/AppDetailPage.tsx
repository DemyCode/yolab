import { AnimatedList, Collapse, Swap } from "@/components/motion";
import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowLeft, RefreshCw } from "lucide-react";
import { Page } from "@/components/AppShell";
import { AppAccess } from "@/components/AppAccess";
import { AppIconTile } from "@/components/AppIcon";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Sheet } from "@/components/ui/sheet";
import {
  Banner,
  ServiceTrouble,
  Skeleton,
  Spinner,
} from "@/components/ui/feedback";
import { Input, Select, Switch } from "@/components/ui/input";
import {
  DisclosureRow,
  Row,
  RowAction,
  Section,
  ValueRow,
} from "@/components/ui/list";
import { api, streamEvents } from "@/lib/api";
import { formatDateTime, relativeTime } from "@/lib/format";
import { useApi } from "@/lib/useResource";
import { usedBy, usesOf } from "@/lib/connections";
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
  podProblem,
  podStatus,
  waitNote,
  type AppState,
  type RestoreRecord,
  type StatusTone,
} from "@/lib/apps";
import { taglineFor } from "@/catalog/meta";
import { cn } from "@/lib/utils";
import type {
  AppInfo,
  CatalogApp,
  DomainResponse,
  PodInfo,
} from "@/types/apps";

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

function useNow(everyMs: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), everyMs);
    return () => clearInterval(id);
  }, [everyMs]);
  return now;
}

const POD_REFRESH_MS = 5000;
const MAX_LOG_LINES = 1000;

function TechnicalDetails({ app }: { app: AppInfo }) {
  const [pods, setPods] = useState<PodInfo[] | null>(null);
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
    setLogs((l) => (l ? { ...l, live: false } : null));
  }

  function startLogs(pod: string) {
    logStream.current?.abort();
    const ctrl = new AbortController();
    logStream.current = ctrl;
    setLogs({ pod, lines: [], live: true });
    const settle = () => {
      if (logStream.current === ctrl) {
        setLogs((l) => (l && l.pod === pod ? { ...l, live: false } : l));
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
    )
      .then(settle)
      .catch(settle);
  }

  useEffect(() => () => logStream.current?.abort(), []);

  useEffect(() => {
    const el = logBox.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [logs?.lines.length]);

  useEffect(() => {
    let cancelled = false;
    const load = () =>
      void api
        .get<PodInfo[]>(`/api/apps/${app.instance_name}/pods`)
        .then((p) => {
          if (!cancelled) setPods(p);
        })
        .catch(() => {
          if (!cancelled) setPods((prev) => prev ?? []);
        });
    load();
    const timer = setInterval(load, POD_REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [app.instance_name]);

  const settings = Object.entries(app.config ?? {});

  return (
    <>
      <div>
        <h3 className="mb-1.5 text-xs font-medium text-fg-muted">
          Running parts
        </h3>
        {!pods ? (
          <Spinner className="h-4 w-4" />
        ) : pods.length === 0 ? (
          <p className="text-sm text-fg-muted">Nothing running right now.</p>
        ) : (
          <ul className="divide-y divide-border rounded-control border border-border">
            <AnimatedList
              as="li"
              items={pods}
              keyOf={(pod) => pod.name}
              itemClassName="px-3 py-1.5"
            >
              {(pod) => {
                const status = podProblem(pod) ?? podStatus(pod);
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
                        {logs?.pod === pod.name && logs.live ? "Stop" : "Logs"}
                      </RowAction>
                    </div>
                    <p className="mt-0.5 pl-4 text-xs text-fg-subtle">
                      {pod.node
                        ? `On ${pod.node}`
                        : "Not placed on a machine yet"}
                      {pod.restarts ? ` · restarted ${pod.restarts}×` : ""}
                    </p>
                    {(pod.containers ?? []).length > 0 && (
                      <ul className="mt-1 space-y-0.5 pl-4">
                        {(pod.containers ?? []).map((c) => (
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
                                <span className="text-fg-subtle"> (setup)</span>
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
                    {(pod.events ?? []).length > 0 && (
                      <ul className="mt-1.5 space-y-0.5 border-l border-border pl-3 ml-4">
                        {(pod.events ?? []).map((e, i) => (
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
                ? "Connected — waiting for this app to print something…"
                : "This app printed nothing."}
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

interface RestoreSnapshot {
  id: string;
  time: string;
}

function RestoreSheet({
  instanceName,
  name,
  open,
  onClose,
  onStarted,
}: {
  instanceName: string;
  name: string;
  open: boolean;
  onClose: () => void;
  onStarted: (snapshotTime: string | null) => void;
}) {
  const [snapshots, setSnapshots] = useState<RestoreSnapshot[] | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setSelected(null);
    setError(null);
    setSnapshots(null);
    setLoadFailed(false);
    void api
      .get<{ snapshots?: RestoreSnapshot[] }>(
        `/api/backups/snapshots?namespace=${encodeURIComponent(`yolab-${instanceName}`)}`,
      )
      .then((d) => {
        if (cancelled) return;
        const snaps = (d.snapshots ?? [])
          .slice()
          .sort(
            (a, b) => new Date(b.time).getTime() - new Date(a.time).getTime(),
          );
        setSnapshots(snaps);
        if (snaps.length > 0) setSelected(snaps[0].id);
      })
      .catch(() => {
        if (cancelled) return;
        setLoadFailed(true);
        setSnapshots([]);
      });
    return () => {
      cancelled = true;
    };
  }, [open, instanceName]);

  async function confirm() {
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/backups/restore", {
        namespace: `yolab-${instanceName}`,
        snapshot_id: selected,
      });
      onStarted(snapshots?.find((s) => s.id === selected)?.time ?? null);
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "The restore did not start.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Sheet
      open={open}
      onClose={onClose}
      title={`Restore ${name}`}
      subtitle="Its files and settings go back to the backup you pick. Anything newer is lost."
    >
      {snapshots === null ? (
        <div className="flex items-center gap-2 py-4 text-sm text-fg-muted">
          <Spinner className="h-4 w-4" />
          Looking for backups…
        </div>
      ) : loadFailed ? (
        <p className="py-4 text-sm text-fg-muted">
          The backups could not be listed right now. Close this and try again in
          a moment.
        </p>
      ) : snapshots.length === 0 ? (
        <p className="py-4 text-sm text-fg-muted">
          There is no backup of this app yet, so there is nothing to go back to.
        </p>
      ) : (
        <div className="divide-y divide-border rounded-card border border-border">
          <AnimatedList
            items={snapshots.map((s, i) => ({ s, i }))}
            keyOf={({ s }) => s.id}
          >
            {({ s, i }) => (
              <label className="flex cursor-pointer items-center gap-3 px-4 py-3 hover:bg-surface-2">
                <input
                  type="radio"
                  name="restore-snapshot"
                  checked={selected === s.id}
                  onChange={() => setSelected(s.id)}
                  className="accent-primary"
                />
                <span className="flex-1 text-sm text-fg">
                  {formatDateTime(s.time)}
                </span>
                {i === 0 && (
                  <span className="text-xs text-fg-subtle">Latest</span>
                )}
              </label>
            )}
          </AnimatedList>
        </div>
      )}

      {error && <p className="mt-3 text-sm text-danger">{error}</p>}

      <div className="mt-5 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
        <Button variant="secondary" onClick={onClose} disabled={busy}>
          Cancel
        </Button>
        <Button
          variant="danger"
          onClick={() => void confirm()}
          loading={busy}
          disabled={!selected}
        >
          Restore
        </Button>
      </div>
    </Sheet>
  );
}

const BACKUP_PRESETS: { label: string; cron: string }[] = [
  { label: "Every day at 03:00", cron: "0 3 * * *" },
  { label: "Every 6 hours", cron: "0 */6 * * *" },
  { label: "Every Sunday at 03:00", cron: "0 3 * * 0" },
  { label: "On the 1st of each month", cron: "0 3 1 * *" },
];

function BackupsSection({
  app,
  canBackUp,
  canRestore,
  onRestore,
  onChanged,
}: {
  app: AppInfo;
  canBackUp: boolean;
  canRestore: boolean;
  onRestore: () => void;
  onChanged: () => Promise<unknown>;
}) {
  const [pending, setPending] = useState<{
    enabled: boolean;
    schedule: string;
  } | null>(null);
  const [starting, setStarting] = useState(false);
  const [custom, setCustom] = useState(
    !BACKUP_PRESETS.some((p) => p.cron === app.backup.schedule),
  );
  const [draft, setDraft] = useState(app.backup.schedule);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);

  const enabled = pending?.enabled ?? app.backup.enabled;
  const schedule = pending?.schedule ?? app.backup.schedule;
  const running = starting || app.backup.running;

  async function save(next: { enabled: boolean; schedule: string }) {
    setPending(next);
    setNote(null);
    try {
      await api.put(`/api/apps/${app.instance_name}/backup`, next);
      setNote({ ok: true, text: "Saved" });
      await onChanged();
    } catch (e) {
      setNote({
        ok: false,
        text: e instanceof Error ? e.message : "Could not save that.",
      });
    } finally {
      setPending(null);
    }
  }

  async function backupNow() {
    setStarting(true);
    setNote(null);
    try {
      await api.post(`/api/backups/apps/yolab-${app.instance_name}/run-now`);
      await onChanged();
    } catch (e) {
      setNote({
        ok: false,
        text: e instanceof Error ? e.message : "Could not start a backup.",
      });
    } finally {
      setStarting(false);
    }
  }

  const last = app.backup.last_ok_at;

  return (
    <Section
      title="Backups"
      action={
        note && (
          <span
            className={cn("text-xs", note.ok ? "text-success" : "text-danger")}
            role="status"
          >
            {note.text}
          </span>
        )
      }
    >
      <Row
        label="Back up automatically"
        trailing={
          <Switch
            checked={enabled}
            onChange={(v) => void save({ enabled: v, schedule })}
            label="Back up automatically"
          />
        }
      />
      {enabled && (
        <Row
          label="How often"
          trailing={
            <Select
              value={custom ? "__custom" : schedule}
              onChange={(e) => {
                if (e.target.value === "__custom") {
                  setDraft(schedule);
                  setCustom(true);
                  return;
                }
                setCustom(false);
                void save({ enabled, schedule: e.target.value });
              }}
              aria-label="How often"
              className="h-9 w-auto max-w-[14rem]"
            >
              {BACKUP_PRESETS.map((p) => (
                <option key={p.cron} value={p.cron}>
                  {p.label}
                </option>
              ))}
              <option value="__custom">Custom…</option>
            </Select>
          }
        >
          {custom && (
            <div className="mt-3">
              <div className="flex gap-2">
                <Input
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  aria-label="Custom schedule"
                  spellCheck={false}
                  className="h-9 font-mono"
                />
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => void save({ enabled, schedule: draft })}
                  disabled={!draft.trim() || draft === schedule}
                >
                  Save
                </Button>
              </div>
              <p className="mt-1.5 text-xs text-fg-subtle">
                Minute, hour, day, month, weekday, in the server&rsquo;s time.{" "}
                <code className="font-mono">0 3 * * *</code> is every day at
                03:00.
              </p>
            </div>
          )}
        </Row>
      )}
      <Row
        label="Last backup"
        detail={
          running ? (
            "Backing up now…"
          ) : last ? (
            <span title={formatDateTime(last)}>{relativeTime(last)}</span>
          ) : (
            "Not backed up yet"
          )
        }
        trailing={
          canBackUp ? (
            <RowAction onClick={() => void backupNow()} disabled={running}>
              {running ? "Backing up…" : "Back up now"}
            </RowAction>
          ) : null
        }
      />
      {canRestore && (
        <Row
          label="Restore from a backup"
          detail="Go back to an earlier copy of its files and settings"
          onClick={onRestore}
        />
      )}
    </Section>
  );
}

type Notice =
  | { tone: "success"; title: string; body?: string }
  | { tone: "error"; title: string; body: string };

export function AppDetailPage() {
  const { instanceName } = useParams<{ instanceName: string }>();
  const navigate = useNavigate();

  const apps = useApi<AppInfo[]>("apps", "/api/apps", { pollMs: 10_000 });
  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const domain = useApi<DomainResponse>("domain", "/api/tunnel/domain");

  const [confirmRemove, setConfirmRemove] = useState(false);
  const [working, setWorking] = useState<null | "update" | "remove">(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [restoreOpen, setRestoreOpen] = useState(false);
  const [restore, setRestore] = useState<RestoreRecord | null>(null);
  const [restoreSeen, setRestoreSeen] = useState<string | null>(null);

  const app = apps.data?.find((a) => a.instance_name === instanceName);
  const state = app ? appState(app) : "starting";
  const now = useNow(5_000);

  useEffect(() => {
    if (!instanceName) return;
    let cancelled = false;
    async function poll() {
      try {
        const list = await api.get<RestoreRecord[]>("/api/backups/restores");
        if (!cancelled) {
          setRestore(latestRestore(list, `yolab-${instanceName}`, Date.now()));
        }
        // eslint-disable-next-line no-empty
      } catch {}
    }
    void poll();
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
      <Page title="App not found">
        <p className="text-sm text-fg-muted">
          There is no app called “{instanceName}” on this server. It may have
          been removed.
        </p>
        <Link
          to="/"
          className="mt-4 inline-flex items-center gap-1.5 text-sm text-primary"
        >
          <ArrowLeft className="h-4 w-4" />
          Back to my apps
        </Link>
      </Page>
    );
  }

  const entry = catalogEntry(app, catalog.data ?? []);
  const name = appDisplayName(app, catalog.data ?? [], apps.data ?? []);
  const links = appLinks(app, domain.data?.domain ?? "");
  const restoring = restore?.state === "running";
  const actions = availableActions(state, restoring);
  const newer = newerVersion(app, entry);
  const version = app.chart_version || entry?.chart_version || "";
  const stem = instanceStem(app);
  const current = app;

  async function remove() {
    if (!app) return;
    setWorking("remove");
    setNotice(null);
    try {
      await api.del(`/api/apps/${app.instance_name}`);
      navigate("/");
    } catch (e) {
      setNotice({
        tone: "error",
        title: "It could not be removed",
        body:
          e instanceof Error ? e.message : "The server did not accept that.",
      });
      setWorking(null);
    }
  }

  async function update() {
    if (!app) return;
    setWorking("update");
    setNotice(null);
    let reached = "";
    const result = await streamEvents(
      `/api/apps/${app.instance_name}/update`,
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
        body: result.error ?? "Your app was left as it was.",
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
          This app and its files are being deleted.
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
                onClick={() => setRestoreOpen(true)}
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
          {restoreDone.finished_at
            ? `, finished ${relativeTime(restoreDone.finished_at)}`
            : ""}
          .
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
        My apps
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

      {!quiet && (
        <AppAccess
          instanceName={app.instance_name}
          appReady={state === "ready"}
          links={links}
        />
      )}

      {!quiet && (
        <BackupsSection
          app={app}
          canBackUp={actions.has("backup")}
          canRestore={actions.has("restore")}
          onRestore={() => setRestoreOpen(true)}
          onChanged={() => apps.refresh()}
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
              {newer && actions.has("update") && (
                <span className="font-sans text-fg-subtle">
                  {" "}
                  · {newer} available
                </span>
              )}
            </span>
          }
          trailing={
            newer && actions.has("update") ? (
              <RowAction
                onClick={() => void update()}
                disabled={working === "update"}
              >
                {working === "update" ? "Updating…" : `Update to ${newer}`}
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
        {usesOf(app).length > 0 && (
          <Row
            label="Uses"
            detail={usesOf(app).map((name, i) => (
              <span key={name}>
                {i > 0 && ", "}
                <Link to={`/app/${name}`} className="text-primary">
                  {name}
                </Link>
              </span>
            ))}
          />
        )}
        {usedBy(app, apps.data ?? []).length > 0 && (
          <Row
            label="Used by"
            detail={usedBy(app, apps.data ?? []).map((name, i) => (
              <span key={name}>
                {i > 0 && ", "}
                <Link to={`/app/${name}`} className="text-primary">
                  {name}
                </Link>
              </span>
            ))}
          />
        )}
        {app.group && (
          <Row
            label="Group"
            detail={`Part of ${app.group.title}`}
            onClick={() => navigate(`/group/${app.group?.name}`)}
          />
        )}
        {stem !== app.app_id && <Row label="Name" detail={stem} />}
        {app.instance_id && (
          <ValueRow label="ID" value={app.instance_id} copy />
        )}
        {actions.has("settings") && (
          <Row
            label="Change settings"
            detail="Point it at another app, change what it was installed with"
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
            detail="Deletes its files and settings. Backups you already have are kept."
            danger
            onClick={() => setConfirmRemove(true)}
          />
        </Section>
      )}

      <RestoreSheet
        instanceName={app.instance_name}
        name={name}
        open={restoreOpen}
        onClose={() => setRestoreOpen(false)}
        onStarted={() => {
          setRestoreSeen(null);
          setRestore({
            id: "pending",
            namespace: `yolab-${app.instance_name}`,
            started_at: new Date().toISOString(),
            state: "running",
          });
        }}
      />

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
            This deletes {name} and everything stored in it: files, settings and
            history. Backups you already have are kept, so it can come back from
            one, but nothing added since the last backup survives.
          </>
        }
      />
    </Page>
  );
}
