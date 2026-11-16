import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Row, RowAction, Section } from "@/components/ui/list";
import { ConfirmDialog, Sheet } from "@/components/ui/sheet";
import { Spinner } from "@/components/ui/feedback";
import { AnimatedList } from "@/components/motion";
import { api } from "@/lib/api";
import { formatBytes, formatDateTime } from "@/lib/format";
import {
  budgetToBytes,
  bytesToBudget,
  canDelete,
  pointPath,
  pointsPath,
  sortRestorePoints,
  usageLabel,
} from "@/lib/backups";
import type { RestorePoint, RestorePoints } from "@/lib/backups";
import { cn } from "@/lib/utils";

function message(e: unknown, fallback: string): string {
  return e instanceof Error ? e.message : fallback;
}

function useCopies(namespace: string, open: boolean) {
  const [data, setData] = useState<RestorePoints | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const d = await api.get<RestorePoints>(pointsPath(namespace));
      setData({ ...d, points: sortRestorePoints(d.points ?? []) });
      setError(null);
    } catch (e) {
      setError(message(e, "Its backups could not be listed."));
    }
  }, [namespace]);

  useEffect(() => {
    if (!open) return;
    setData(null);
    void load();
  }, [open, load]);

  return { data, error, reload: load };
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
      setError(message(e, "The budget was not saved."));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Row
      label="Space for its backups"
      detail={
        copies.budget_set
          ? "When its copies take more than this, the oldest ones are deleted. The newest copy and the ones you keep always stay."
          : "Chosen for you: twice the size of its newest copy, at least 1 GB. Set your own to keep more or fewer copies."
      }
    >
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <div className="flex items-center gap-2">
          <Input
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            inputMode="decimal"
            aria-label="Budget in GB"
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

function PointRow({
  point,
  busy,
  onKeep,
  onDelete,
}: {
  point: RestorePoint;
  busy: boolean;
  onKeep: (keep: boolean) => void;
  onDelete: () => void;
}) {
  const details = [
    point.newest ? "Newest" : null,
    point.keep ? "Kept" : null,
    point.older ? "Older backup" : null,
    point.added_bytes != null ? `${formatBytes(point.added_bytes)} new` : null,
  ].filter(Boolean);

  return (
    <div className="flex items-center gap-3 px-4 py-3">
      <div className="min-w-0 flex-1">
        <p className="font-mono text-sm tabular-nums text-fg">
          {formatDateTime(point.time)}
        </p>
        {details.length > 0 && (
          <p
            className={cn(
              "mt-0.5 text-xs",
              point.keep ? "text-primary" : "text-fg-subtle",
            )}
          >
            {details.join(" · ")}
          </p>
        )}
      </div>
      <RowAction onClick={() => onKeep(!point.keep)} disabled={busy}>
        {point.keep ? "Stop keeping" : "Keep"}
      </RowAction>
      {canDelete(point) && (
        <RowAction onClick={onDelete} disabled={busy}>
          Delete
        </RowAction>
      )}
    </div>
  );
}

function DeletePoint({
  namespace,
  point,
  onClose,
  onDeleted,
}: {
  namespace: string;
  point: RestorePoint | null;
  onClose: () => void;
  onDeleted: () => Promise<void>;
}) {
  const [frees, setFrees] = useState<
    { id: string; bytes: number | null } | "failed" | null
  >(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const id = point?.snapshot_id ?? null;

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    setFrees(null);
    setError(null);
    api
      .get<{ frees_bytes: number | null }>(`${pointPath(namespace, id)}/frees`)
      .then((d) => {
        if (!cancelled) setFrees({ id, bytes: d.frees_bytes });
      })
      .catch(() => {
        if (!cancelled) setFrees("failed");
      });
    return () => {
      cancelled = true;
    };
  }, [namespace, id]);

  async function confirm() {
    if (!id) return;
    setBusy(true);
    setError(null);
    try {
      await api.del(pointPath(namespace, id));
      await onDeleted();
      onClose();
    } catch (e) {
      setError(message(e, "That copy was not deleted."));
    } finally {
      setBusy(false);
    }
  }

  const known = frees !== null && frees !== "failed" && frees.id === id;

  return (
    <ConfirmDialog
      open={point !== null}
      onClose={onClose}
      onConfirm={() => void confirm()}
      title="Delete this copy?"
      destructive
      confirmLabel="Delete it"
      busy={busy}
      body={
        <>
          The copy from {point ? formatDateTime(point.time) : ""} is deleted for
          good. You can no longer go back to it.
          <p className="mt-3 text-sm text-fg-muted">
            {frees === "failed"
              ? "How much space it frees could not be worked out."
              : !known
                ? "Working out how much space it frees…"
                : frees.bytes === null
                  ? "It is an older backup; the space it frees is known once it is gone."
                  : frees.bytes === 0
                    ? "It frees almost nothing: its files are shared with other copies."
                    : `It frees about ${formatBytes(frees.bytes)}.`}
          </p>
          {error && <p className="mt-2 text-sm text-danger">{error}</p>}
        </>
      }
    />
  );
}

export function BackupCopies({
  namespace,
  name,
  open,
  removed,
  onClose,
  onChanged,
}: {
  namespace: string;
  name: string;
  open: boolean;
  removed?: boolean;
  onClose: () => void;
  onChanged?: () => void;
}) {
  const { data, error, reload } = useCopies(namespace, open);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<RestorePoint | null>(null);
  const [deletingAll, setDeletingAll] = useState(false);

  const changed = useCallback(async () => {
    await reload();
    onChanged?.();
  }, [reload, onChanged]);

  async function keep(point: RestorePoint, next: boolean) {
    setBusy(true);
    setNote(null);
    try {
      await api.put(`${pointPath(namespace, point.snapshot_id)}/keep`, {
        keep: next,
      });
      await changed();
    } catch (e) {
      setNote(message(e, "That did not change."));
    } finally {
      setBusy(false);
    }
  }

  async function deleteAll() {
    setBusy(true);
    setNote(null);
    try {
      await api.del(`/api/backups/apps/${encodeURIComponent(namespace)}`);
      setDeletingAll(false);
      onChanged?.();
      onClose();
    } catch (e) {
      setNote(message(e, "Its backups were not deleted."));
    } finally {
      setBusy(false);
    }
  }

  const usage = data ? usageLabel(data.size_bytes, data.budget_bytes) : null;

  return (
    <>
      <Sheet
        open={open && deleting === null && !deletingAll}
        onClose={onClose}
        title={`Backups of ${name}`}
        subtitle={
          usage
            ? `Its copies take ${usage}.`
            : "Each copy is a moment you can bring this service back to."
        }
      >
        {data === null && !error ? (
          <div className="flex items-center gap-2 py-4 text-sm text-fg-muted">
            <Spinner className="h-4 w-4" />
            Looking for its backups…
          </div>
        ) : error && data === null ? (
          <p className="py-4 text-sm text-fg-muted">{error}</p>
        ) : data && data.points.length === 0 ? (
          <p className="py-4 text-sm text-fg-muted">
            There is no copy of this service.
          </p>
        ) : data ? (
          <>
            {data.over_budget && (
              <p className="mb-3 text-sm text-warning">
                Its copies take more than its budget, but only the newest copy and
                the ones you keep are left. Raise the budget or stop keeping a
                copy.
              </p>
            )}
            <div className="divide-y divide-border rounded-card border border-border">
              <AnimatedList items={data.points} keyOf={(p) => p.snapshot_id}>
                {(p) => (
                  <PointRow
                    point={p}
                    busy={busy}
                    onKeep={(next) => void keep(p, next)}
                    onDelete={() => setDeleting(p)}
                  />
                )}
              </AnimatedList>
            </div>
            {data.points.some((p) => p.added_bytes != null) && (
              <p className="mt-2 text-xs text-fg-subtle">
                &ldquo;New&rdquo; is what that copy added; files unchanged since
                the copy before it are stored once.
              </p>
            )}

            {!removed && (
              <Section title="Budget">
                <Budget
                  key={`${data.budget_bytes ?? "auto"}-${data.budget_set ? 1 : 0}`}
                  namespace={namespace}
                  copies={data}
                  onSaved={changed}
                />
              </Section>
            )}

            {removed && (
              <Section title="Delete">
                <Row
                  label="Delete all its backups"
                  detail="This service is no longer installed. Its copies keep taking space until you delete them."
                  danger
                  onClick={() => setDeletingAll(true)}
                />
              </Section>
            )}
          </>
        ) : null}

        {note && <p className="mt-3 text-sm text-danger">{note}</p>}
      </Sheet>

      <DeletePoint
        namespace={namespace}
        point={deleting}
        onClose={() => setDeleting(null)}
        onDeleted={changed}
      />

      <ConfirmDialog
        open={deletingAll}
        onClose={() => setDeletingAll(false)}
        onConfirm={() => void deleteAll()}
        title={`Delete every backup of ${name}?`}
        destructive
        confirmLabel="Delete them all"
        busy={busy}
        body={
          <>
            {data?.points.length ?? 0} copies are deleted for good
            {data?.size_bytes != null
              ? `, freeing about ${formatBytes(data.size_bytes)}`
              : ""}
            . {name} can no longer be added back from a backup.
            {note && <p className="mt-2 text-sm text-danger">{note}</p>}
          </>
        }
      />
    </>
  );
}
