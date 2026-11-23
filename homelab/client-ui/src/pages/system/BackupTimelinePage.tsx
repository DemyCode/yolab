import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowLeft } from "lucide-react";
import { Page } from "@/components/AppShell";
import { BackupTimeline } from "@/components/BackupTimeline";
import { ScheduleRow, SpaceRow } from "@/components/BackupPolicyRows";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/input";
import { Row, RowAction, Section } from "@/components/ui/list";
import { ConfirmDialog } from "@/components/ui/sheet";
import { Banner, Spinner } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { formatBytes, formatDateTime } from "@/lib/format";
import { useApi } from "@/lib/useResource";
import { useNow } from "@/lib/useNow";
import {
  ageLabel,
  copiesLabel,
  pointPath,
  pointsPath,
  restorePath,
  serviceBanner,
  serviceName,
  sortRestorePoints,
  spaceToFit,
  usageLabel,
} from "@/lib/backups";
import type {
  ProtectedApp,
  RestorePoint,
  RestorePoints,
} from "@/lib/backups";
import { DEFAULT_SPACE_BYTES } from "@/lib/settings";
import type { AppDefinition } from "@/types/apps";
import { cn } from "@/lib/utils";

function message(e: unknown, fallback: string): string {
  return e instanceof Error ? e.message : fallback;
}

const LIST_FAILED = "Its backups could not be listed.";

function useCopies(namespace: string) {
  const [data, setData] = useState<RestorePoints | null>(null);
  const [error, setError] = useState<string | null>(null);

  const fetchCopies = useCallback(async () => {
    const d = await api.get<RestorePoints>(pointsPath(namespace));
    return { ...d, points: sortRestorePoints(d.points ?? []) };
  }, [namespace]);

  const load = useCallback(async () => {
    try {
      setData(await fetchCopies());
      setError(null);
    } catch (e) {
      setError(message(e, LIST_FAILED));
    }
  }, [fetchCopies]);

  useEffect(() => {
    let cancelled = false;
    fetchCopies()
      .then((d) => {
        if (cancelled) return;
        setData(d);
        setError(null);
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(message(e, LIST_FAILED));
      });
    return () => {
      cancelled = true;
    };
  }, [fetchCopies]);

  return { data, error, reload: load };
}

async function chartOf(namespace: string, snapshotId: string) {
  const def = await api.get<AppDefinition>(
    `/api/backups/apps/${encodeURIComponent(namespace)}/definition?snapshot_id=${encodeURIComponent(snapshotId)}`,
  );
  if (!def.app_id) {
    throw new Error("That backup does not say which service it is.");
  }
  return def.app_id;
}

function ConfirmDelete({
  namespace,
  point,
  onClose,
  onConfirm,
}: {
  namespace: string;
  point: RestorePoint | null;
  onClose: () => void;
  onConfirm: (point: RestorePoint) => void;
}) {
  const [frees, setFrees] = useState<{
    id: string;
    bytes: number | null | "failed";
  } | null>(null);
  const id = point?.snapshot_id ?? null;

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    api
      .get<{ frees_bytes: number | null }>(`${pointPath(namespace, id)}/frees`)
      .then((d) => {
        if (!cancelled) setFrees({ id, bytes: d.frees_bytes });
      })
      .catch(() => {
        if (!cancelled) setFrees({ id, bytes: "failed" });
      });
    return () => {
      cancelled = true;
    };
  }, [namespace, id]);

  const current = frees !== null && frees.id === id ? frees.bytes : undefined;

  return (
    <ConfirmDialog
      open={point !== null}
      onClose={onClose}
      onConfirm={() => {
        if (point) onConfirm(point);
        onClose();
      }}
      title="Delete this copy?"
      destructive
      confirmLabel="Delete it"
      body={
        <>
          The copy from {point ? formatDateTime(point.time) : ""} is deleted for
          good, in the background. You can no longer go back to it.
          <p className="mt-3 text-sm text-fg-muted">
            {current === undefined
              ? "Working out how much space it frees…"
              : current === "failed"
                ? "How much space it frees could not be worked out."
                : current === null
                  ? "The space it frees is known once it is gone."
                  : current === 0
                    ? "It frees almost nothing: its files are shared with other copies."
                    : `It frees about ${formatBytes(current)}.`}
          </p>
        </>
      }
    />
  );
}

function CopyRow({
  namespace,
  point,
  selected,
  now,
  onSelect,
  onDelete,
}: {
  namespace: string;
  point: RestorePoint;
  selected: boolean;
  now: number;
  onSelect: () => void;
  onDelete: () => void;
}) {
  const navigate = useNavigate();
  const [opening, setOpening] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);

  async function restore() {
    setOpening(true);
    setFailed(null);
    try {
      const chart = await chartOf(namespace, point.snapshot_id);
      navigate(restorePath(chart, namespace, point.snapshot_id));
    } catch (e) {
      setFailed(message(e, "That copy could not be opened."));
      setOpening(false);
    }
  }

  const detail = [
    ageLabel(point.time, now),
    point.newest ? "Newest" : null,
    point.added_bytes != null
      ? `${formatBytes(point.added_bytes)} not in the copy before`
      : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <Row
      label={
        <button
          type="button"
          onClick={onSelect}
          aria-pressed={selected}
          className={cn(
            "font-mono tabular-nums hover:underline",
            selected && "text-primary",
          )}
        >
          {formatDateTime(point.time)}
        </button>
      }
      detail={detail}
      trailing={
        <RowAction onClick={() => void restore()} disabled={opening}>
          {opening ? "Opening…" : "Restore"}
        </RowAction>
      }
    >
      {(selected || failed) && (
        <div className="mt-2 flex flex-wrap items-center gap-2">
          {selected && (
            <RowAction danger onClick={onDelete}>
              Delete this copy
            </RowAction>
          )}
          {failed && <span className="text-sm text-danger">{failed}</span>}
        </div>
      )}
    </Row>
  );
}

export function BackupTimelinePage() {
  const { namespace = "" } = useParams<{ namespace: string }>();
  const navigate = useNavigate();
  const now = useNow(60_000);
  const { data, error, reload } = useCopies(namespace);
  const running = useApi<{ configured: boolean; apps: ProtectedApp[] }>(
    "protected-apps",
    "/api/backups/protected",
    { pollMs: 10_000 },
  );
  const app = running.data?.apps.find((a) => a.namespace === namespace);
  const name = app?.instance_name ?? serviceName(namespace);
  const installed = app !== undefined;

  const [chosen, setChosen] = useState<string | null>(null);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const [confirming, setConfirming] = useState<RestorePoint | null>(null);
  const [deleting, setDeleting] = useState<string[]>([]);
  const [deletingAll, setDeletingAll] = useState(false);
  const [confirmAll, setConfirmAll] = useState(false);
  const [starting, setStarting] = useState(false);
  const [saving, setSaving] = useState(false);
  const queue = useRef<Promise<void>>(Promise.resolve());

  const points = (data?.points ?? []).filter(
    (p) => !deleting.includes(p.snapshot_id),
  );
  const selected =
    points.find((p) => p.snapshot_id === chosen) ?? points[0] ?? null;
  const space = data?.budget_bytes ?? DEFAULT_SPACE_BYTES;
  const backingUp =
    starting || app?.state === "running" || app?.state === "queued";
  const banner = serviceBanner(app, data, now);
  const usage = data ? usageLabel(data.size_bytes, data.budget_bytes) : null;

  function fail(e: unknown, fallback: string) {
    setNote({ ok: false, text: message(e, fallback) });
  }

  function remove(point: RestorePoint) {
    setNote(null);
    setDeleting((d) => [...d, point.snapshot_id]);
    queue.current = queue.current.then(async () => {
      try {
        await api.del(pointPath(namespace, point.snapshot_id));
        await reload();
      } catch (e) {
        fail(e, `The copy from ${formatDateTime(point.time)} was not deleted.`);
      } finally {
        setDeleting((d) => d.filter((id) => id !== point.snapshot_id));
      }
    });
  }

  function removeAll() {
    setNote(null);
    setDeletingAll(true);
    queue.current = queue.current.then(async () => {
      try {
        await api.del(`/api/backups/apps/${encodeURIComponent(namespace)}`);
        if (installed) {
          await reload();
          setDeletingAll(false);
        } else {
          navigate("/system/backups");
        }
      } catch (e) {
        fail(e, "Its backups were not deleted.");
        setDeletingAll(false);
      }
    });
  }

  async function backUpNow() {
    setStarting(true);
    setNote(null);
    try {
      await api.post(
        `/api/backups/apps/${encodeURIComponent(namespace)}/run-now`,
      );
      await running.refresh();
    } catch (e) {
      fail(e, "Could not start a backup.");
    } finally {
      setStarting(false);
    }
  }

  async function savePolicy(next: { enabled: boolean; schedule: string }) {
    if (!app) return;
    setSaving(true);
    setNote(null);
    try {
      await api.put(`/api/apps/${app.instance_name}/backup`, next);
      await running.refresh();
      setNote({ ok: true, text: "Saved" });
    } catch (e) {
      fail(e, "That was not saved.");
    } finally {
      setSaving(false);
    }
  }

  async function saveSpace(bytes: number) {
    setSaving(true);
    setNote(null);
    try {
      await api.put(
        `/api/backups/apps/${encodeURIComponent(namespace)}/budget`,
        { budget_bytes: bytes },
      );
      await reload();
      setNote({ ok: true, text: "Saved" });
    } catch (e) {
      fail(e, "The space was not saved.");
    } finally {
      setSaving(false);
    }
  }

  return (
    <Page>
      <Link
        to="/system/backups"
        className="mb-5 inline-flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
      >
        <ArrowLeft className="h-4 w-4" />
        Backups
      </Link>
      <header className="mb-6">
        <h1 className="font-display text-[1.75rem] leading-tight text-fg md:text-4xl">
          {name}
        </h1>
        <p className="mt-1 text-sm text-fg-muted">
          {[
            data ? copiesLabel(points.length) : null,
            usage,
            !installed && running.data ? "Removed" : null,
            installed && app && !app.enabled ? "Backups off" : null,
          ]
            .filter(Boolean)
            .join(" · ") ||
            "Each copy is a moment you can bring this service back to."}
        </p>
      </header>

      {banner?.kind === "failed" && (
        <div className="mb-6">
          <Banner
            tone="error"
            title="The last backup failed"
            action={
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void backUpNow()}
                loading={backingUp}
              >
                Try again
              </Button>
            }
          >
            {banner.error}
          </Banner>
        </div>
      )}
      {banner?.kind === "over" && (
        <div className="mb-6">
          <Banner
            tone="warning"
            title="Its newest copy alone needs more than its space"
            action={
              <div className="flex flex-wrap gap-2">
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => void saveSpace(spaceToFit(banner.sizeBytes))}
                  loading={saving}
                >
                  Give it {formatBytes(spaceToFit(banner.sizeBytes))}
                </Button>
                {app?.enabled && (
                  <Button
                    variant="secondary"
                    size="sm"
                    onClick={() =>
                      void savePolicy({
                        enabled: false,
                        schedule: app.schedule,
                      })
                    }
                    loading={saving}
                  >
                    Turn off its backups
                  </Button>
                )}
              </div>
            }
          >
            Its newest copy alone is bigger than its{" "}
            {formatBytes(banner.budgetBytes)} of space, so after each backup
            only that newest copy is kept. Give it more space to keep older
            copies too, or turn its backups off and delete all its backups to
            stop paying for them.
          </Banner>
        </div>
      )}
      {banner?.kind === "stale" && (
        <div className="mb-6">
          <Banner
            tone="warning"
            title="No recent backup"
            action={
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void backUpNow()}
                loading={backingUp}
              >
                Back up now
              </Button>
            }
          >
            Anything changed in {name} since its last copy is not saved anywhere
            else yet.
          </Banner>
        </div>
      )}

      {(deleting.length > 0 || deletingAll) && (
        <p
          className="mb-4 flex items-center gap-2 text-sm text-fg-muted"
          role="status"
        >
          <Spinner className="h-4 w-4" />
          {deletingAll
            ? "Deleting all its copies in the background…"
            : `Deleting ${copiesLabel(deleting.length)} in the background…`}
        </p>
      )}

      {installed && app && (
        <Section
          title="Backing up"
          action={
            note && (
              <span
                className={cn(
                  "text-xs",
                  note.ok ? "text-success" : "text-danger",
                )}
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
                checked={app.enabled}
                onChange={(enabled) =>
                  void savePolicy({ enabled, schedule: app.schedule })
                }
                label="Back up automatically"
                disabled={saving}
              />
            }
          />
          {app.enabled && (
            <ScheduleRow
              key={app.schedule}
              schedule={app.schedule}
              busy={saving}
              onSave={(schedule) =>
                void savePolicy({ enabled: app.enabled, schedule })
              }
            />
          )}
          <Row
            label="Last backup"
            detail={
              backingUp
                ? "Backing up now…"
                : app.last_ok_at
                  ? formatDateTime(app.last_ok_at)
                  : "Not backed up yet"
            }
            trailing={
              <RowAction onClick={() => void backUpNow()} disabled={backingUp}>
                {backingUp ? "Backing up…" : "Back up now"}
              </RowAction>
            }
          />
          {data && (
            <SpaceRow
              key={space}
              label="Space for its backups"
              bytes={space}
              detail="After every backup, copies are deleted until the rest fit, keeping them spread back through time. The newest copy always stays."
              busy={saving}
              onSave={(bytes) => void saveSpace(bytes)}
            />
          )}
        </Section>
      )}

      {!installed && note && (
        <p
          className={cn(
            "mt-3 text-sm",
            note.ok ? "text-success" : "text-danger",
          )}
          role="status"
        >
          {note.text}
        </p>
      )}

      {data === null && !error ? (
        <div className="mt-8 flex items-center gap-2 text-sm text-fg-muted">
          <Spinner className="h-4 w-4" />
          Looking for its backups…
        </div>
      ) : error && data === null ? (
        <p className="mt-8 text-sm text-danger">{error}</p>
      ) : deletingAll ? null : points.length === 0 ? (
        <Section title="Copies">
          <Row
            label="No copy yet"
            detail={
              installed
                ? "Back up now to make the first one."
                : "Every copy of this service has been deleted."
            }
          />
        </Section>
      ) : (
        <>
          {points.length > 1 && (
            <div className="mt-8">
              <BackupTimeline
                points={points}
                selected={selected?.snapshot_id ?? null}
                now={now}
                onSelect={setChosen}
              />
            </div>
          )}
          <Section title="Copies">
            {points.map((p) => (
              <CopyRow
                key={p.snapshot_id}
                namespace={namespace}
                point={p}
                selected={p.snapshot_id === selected?.snapshot_id}
                now={now}
                onSelect={() => setChosen(p.snapshot_id)}
                onDelete={() => setConfirming(p)}
              />
            ))}
          </Section>
        </>
      )}

      {running.data && points.length > 0 && !deletingAll && (
        <Section title="Delete">
          <Row
            label="Delete all its backups"
            detail={
              !installed
                ? "This service is no longer installed. Its copies keep taking space until you delete them."
                : app?.enabled
                  ? "Frees the space its copies take. Its next backup starts a new copy, so turn its backups off first to stop paying for them."
                  : "Frees the space its copies take. Its backups are off, so no new copy is made."
            }
            danger
            onClick={() => setConfirmAll(true)}
          />
        </Section>
      )}

      <ConfirmDelete
        namespace={namespace}
        point={confirming}
        onClose={() => setConfirming(null)}
        onConfirm={remove}
      />

      <ConfirmDialog
        open={confirmAll}
        onClose={() => setConfirmAll(false)}
        onConfirm={() => {
          setConfirmAll(false);
          removeAll();
        }}
        title={`Delete every backup of ${name}?`}
        destructive
        confirmLabel="Delete them all"
        body={
          <>
            Every copy is deleted for good, in the background
            {data?.size_bytes != null
              ? `, freeing about ${formatBytes(data.size_bytes)}`
              : ""}
            . {name} can no longer be brought back to any of them.
          </>
        }
      />
    </Page>
  );
}
