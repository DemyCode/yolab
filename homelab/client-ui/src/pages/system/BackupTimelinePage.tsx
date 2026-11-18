import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ArrowLeft, RotateCcw } from "lucide-react";
import { Page } from "@/components/AppShell";
import { BackupTimeline } from "@/components/BackupTimeline";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Row, RowAction, Section } from "@/components/ui/list";
import { ConfirmDialog } from "@/components/ui/sheet";
import { Banner, Spinner } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { formatBytes, formatDateTime } from "@/lib/format";
import { useApi } from "@/lib/useResource";
import { useNow } from "@/lib/useNow";
import {
  ageLabel,
  budgetToBytes,
  bytesToBudget,
  copiesLabel,
  pointPath,
  pointsPath,
  restorePath,
  serviceName,
  sortRestorePoints,
  usageLabel,
} from "@/lib/backups";
import type { ProtectedApp, RestorePoint, RestorePoints } from "@/lib/backups";
import type { AppDefinition } from "@/types/apps";
import { cn } from "@/lib/utils";

function message(e: unknown, fallback: string): string {
  return e instanceof Error ? e.message : fallback;
}

const LIST_FAILED = "Its backups could not be listed.";
const OVER_SPACE =
  "Its newest copy alone needs more than the space set for its backups, so no copy was deleted. Give it more space.";

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

function Budget({
  namespace,
  copies,
  onSaved,
}: {
  namespace: string;
  copies: RestorePoints;
  onSaved: () => Promise<void>;
}) {
  const [draft, setDraft] = useState(
    copies.budget_bytes ? bytesToBudget(copies.budget_bytes) : "",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const wanted = budgetToBytes(draft);

  async function save(budget: number | null) {
    setBusy(true);
    setError(null);
    try {
      await api.put(
        `/api/backups/apps/${encodeURIComponent(namespace)}/budget`,
        { budget_bytes: budget },
      );
      await onSaved();
    } catch (e) {
      setError(message(e, "The space was not saved."));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Row
      label="Space for its backups"
      detail={
        copies.budget_set
          ? "When its copies need more than this, as many as fit are kept, spaced so the gaps between them double going back in time. The newest copy always stays."
          : "Chosen for you: twice the size of its newest copy, at least 10 GB. Set your own to keep more or fewer copies."
      }
    >
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <div className="flex items-center gap-2">
          <Input
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            inputMode="decimal"
            aria-label="Space in GB"
            className="h-9 w-24 font-mono tabular-nums"
          />
          <span className="text-sm text-fg-muted">GB</span>
        </div>
        <Button
          variant="secondary"
          size="sm"
          onClick={() => void save(wanted)}
          loading={busy}
          disabled={wanted === null || wanted === copies.budget_bytes}
        >
          Save
        </Button>
        {copies.budget_set && (
          <RowAction onClick={() => void save(null)} disabled={busy}>
            Let YoLab choose
          </RowAction>
        )}
      </div>
      {error && <p className="mt-2 text-xs text-danger">{error}</p>}
    </Row>
  );
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

function Moment({
  namespace,
  point,
  onDelete,
}: {
  namespace: string;
  point: RestorePoint;
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

  const tags = [
    point.newest ? "Newest" : null,
    point.added_bytes != null
      ? `${formatBytes(point.added_bytes)} not in the copy before`
      : null,
  ].filter(Boolean);

  return (
    <div className="mt-4 rounded-card border border-border p-5">
      <p className="font-mono text-lg tabular-nums text-fg">
        {formatDateTime(point.time)}
      </p>
      {tags.length > 0 && (
        <p className="mt-1 text-sm text-fg-muted">{tags.join(" · ")}</p>
      )}
      <div className="mt-4 flex flex-wrap items-center gap-2">
        <Button onClick={() => void restore()} loading={opening}>
          <RotateCcw className="h-4 w-4" />
          Restore this copy
        </Button>
        <RowAction onClick={onDelete}>Delete</RowAction>
      </div>
      {failed && <p className="mt-2 text-sm text-danger">{failed}</p>}
    </div>
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
  const [note, setNote] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<RestorePoint | null>(null);
  const [deleting, setDeleting] = useState<string[]>([]);
  const [deletingAll, setDeletingAll] = useState(false);
  const [confirmAll, setConfirmAll] = useState(false);
  const [starting, setStarting] = useState(false);
  const queue = useRef<Promise<void>>(Promise.resolve());

  const points = (data?.points ?? []).filter(
    (p) => !deleting.includes(p.snapshot_id),
  );
  const selected =
    points.find((p) => p.snapshot_id === chosen) ?? points[0] ?? null;

  function remove(point: RestorePoint) {
    setNote(null);
    setDeleting((d) => [...d, point.snapshot_id]);
    queue.current = queue.current.then(async () => {
      try {
        await api.del(pointPath(namespace, point.snapshot_id));
        await reload();
      } catch (e) {
        const what = `The copy from ${formatDateTime(point.time)} was not deleted.`;
        setNote(message(e, what));
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
        navigate("/system/backups");
      } catch (e) {
        setNote(message(e, "Its backups were not deleted."));
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
      setNote(message(e, "Could not start a backup."));
    } finally {
      setStarting(false);
    }
  }

  const saving =
    starting || app?.state === "running" || app?.state === "queued";
  const usage = data ? usageLabel(data.size_bytes, data.budget_bytes) : null;

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
          {data
            ? `${copiesLabel(points.length)}${usage ? ` · ${usage}` : ""}`
            : "Each copy is a moment you can bring this service back to."}
          {!installed && running.data ? " · removed" : ""}
        </p>
      </header>

      {app?.error && (
        <div className="mb-6">
          <Banner tone="error" title="The last backup failed">
            {app.error}
          </Banner>
        </div>
      )}

      {data?.over_budget && (
        <div className="mb-6">
          <Banner tone="warning" title="More than its space">
            {OVER_SPACE}
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

      {data === null && !error ? (
        <div className="flex items-center gap-2 py-4 text-sm text-fg-muted">
          <Spinner className="h-4 w-4" />
          Looking for its backups…
        </div>
      ) : error && data === null ? (
        <p className="py-4 text-sm text-danger">{error}</p>
      ) : points.length === 0 ? (
        <p className="py-4 text-sm text-fg-muted">
          There is no copy of this service yet.
        </p>
      ) : deletingAll ? null : (
        <>
          <BackupTimeline
            points={points}
            selected={selected?.snapshot_id ?? null}
            now={now}
            onSelect={setChosen}
          />
          {selected && (
            <Moment
              namespace={namespace}
              point={selected}
              onDelete={() => setConfirming(selected)}
            />
          )}
          <Section title="Every copy">
            {points.map((p) => (
              <Row
                key={p.snapshot_id}
                onClick={() => setChosen(p.snapshot_id)}
                label={
                  <span
                    className={cn(
                      "font-mono tabular-nums",
                      p.snapshot_id === selected?.snapshot_id && "text-primary",
                    )}
                  >
                    {formatDateTime(p.time)}
                  </span>
                }
                detail={ageLabel(p.time, now)}
              />
            ))}
          </Section>
        </>
      )}

      {note && <p className="mt-3 text-sm text-danger">{note}</p>}

      {installed && (
        <Section title="Backing up">
          <Row
            label="Last backup"
            detail={
              saving
                ? "Backing up now…"
                : app?.last_ok_at
                  ? formatDateTime(app.last_ok_at)
                  : "Not backed up yet"
            }
            trailing={
              <RowAction onClick={() => void backUpNow()} disabled={saving}>
                {saving ? "Backing up…" : "Back up now"}
              </RowAction>
            }
          />
          {data && (
            <Budget
              key={`${data.budget_bytes ?? "auto"}-${data.budget_set ? 1 : 0}`}
              namespace={namespace}
              copies={data}
              onSaved={reload}
            />
          )}
          <Row
            label={`Open ${name}`}
            detail="Its schedule and the rest of its settings"
            onClick={() => navigate(`/app/${name}`)}
          />
        </Section>
      )}

      {!installed && running.data && points.length > 0 && !deletingAll && (
        <Section title="Delete">
          <Row
            label="Delete all its backups"
            detail="This service is no longer installed. Its copies keep taking space until you delete them."
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
            . {name} can no longer be brought back.
          </>
        }
      />
    </Page>
  );
}
