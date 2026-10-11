import { useEffect, useState } from "react";
import { FolderOpen, History, Plus, RotateCcw, Trash2 } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import {
  Banner,
  EmptyState,
  ServiceTrouble,
  Skeleton,
  Spinner,
} from "@/components/ui/feedback";
import { Field, Input, Select, Switch } from "@/components/ui/input";
import { Row, RowAction, Section, ValueRow } from "@/components/ui/list";
import { ConfirmDialog, Sheet } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import { BACKUP_PRESETS, type RestorePoint } from "@/lib/backups";
import { formatDateTime } from "@/lib/format";
import {
  DEFAULT_FOLDER_SCHEDULE,
  DEFAULT_SIZE_GIB,
  SIZE_CHOICES_GIB,
  folderBackupLabel,
  folderSize,
  isDataFolder,
  isRestoring,
  usedByLine,
  type Folder,
} from "@/lib/folders";
import { cn } from "@/lib/utils";
import { useApi } from "@/lib/useResource";

function NewFolderSheet({
  open,
  onClose,
  onCreated,
}: {
  open: boolean;
  onClose: () => void;
  onCreated: () => void;
}) {
  const [title, setTitle] = useState("");
  const [size, setSize] = useState(DEFAULT_SIZE_GIB);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function create() {
    setBusy(true);
    setError(null);
    try {
      await api.post("/api/folders", { title, size_gib: size });
      setTitle("");
      onCreated();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Sheet
      open={open}
      onClose={onClose}
      title="New folder"
      subtitle="A place for your files that several apps can use."
    >
      <div className="space-y-4">
        <Field label="Name" htmlFor="folder-title" error={error}>
          <Input
            id="folder-title"
            value={title}
            autoFocus
            placeholder="Movies & TV"
            onChange={(e) => setTitle(e.target.value)}
          />
        </Field>
        <Field
          label="Room for"
          htmlFor="folder-size"
          help="The most it may hold. It only takes the space its files use."
        >
          <Select
            id="folder-size"
            value={size}
            onChange={(e) => setSize(Number(e.target.value))}
          >
            {SIZE_CHOICES_GIB.map((g) => (
              <option key={g} value={g}>
                {folderSize(`${g}Gi`)}
              </option>
            ))}
          </Select>
        </Field>
      </div>
      <div className="mt-6 flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
        <Button variant="secondary" onClick={onClose} disabled={busy}>
          Cancel
        </Button>
        <Button
          onClick={() => void create()}
          loading={busy}
          disabled={!title.trim()}
        >
          Create folder
        </Button>
      </div>
    </Sheet>
  );
}

function FolderRestorePoints({
  name,
  onRestored,
}: {
  name: string;
  onRestored: () => void;
}) {
  const [points, setPoints] = useState<RestorePoint[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setPoints(null);
    setError(null);
    void api
      .get<{ points?: RestorePoint[] }>(
        `/api/folders/${encodeURIComponent(name)}/points`,
      )
      .then((d) => {
        if (!cancelled) setPoints(d.points ?? []);
      })
      .catch((e: unknown) => {
        if (!cancelled)
          setError(
            e instanceof Error ? e.message : "Your backups could not be read.",
          );
      });
    return () => {
      cancelled = true;
    };
  }, [name]);

  async function restore(snapshotId: string) {
    setBusy(snapshotId);
    setNote(null);
    try {
      const created = await api.post<{ name: string }>(
        `/api/folders/${encodeURIComponent(name)}/restore`,
        { snapshot_id: snapshotId },
      );
      setNote(`Restoring into “${created.name}”. It appears as a new folder.`);
      onRestored();
    } catch (e) {
      setNote(e instanceof Error ? e.message : "That restore could not start.");
    } finally {
      setBusy(null);
    }
  }

  if (error) return <p className="text-sm text-danger">{error}</p>;

  if (points === null) {
    return (
      <div className="flex items-center gap-2 py-4 text-sm text-fg-muted">
        <Spinner />
        Looking through your backups…
      </div>
    );
  }

  if (points.length === 0) {
    return (
      <p className="text-sm text-fg-muted">
        No backup of this folder yet, so there is nothing to go back to.
      </p>
    );
  }

  return (
    <div className="space-y-2">
      {note && (
        <p className="text-sm text-fg-muted" role="status">
          {note}
        </p>
      )}
      <ul className="divide-y divide-border">
        {points.map((point, i) => (
          <li
            key={point.snapshot_id}
            className="flex items-center justify-between gap-3 py-3"
          >
            <div className="flex min-w-0 items-center gap-2.5">
              <History className="h-4 w-4 shrink-0 text-fg-subtle" />
              <div className="min-w-0">
                <div className="truncate text-sm text-fg">
                  {formatDateTime(point.time)}
                </div>
                {i === 0 && (
                  <div className="text-xs text-fg-muted">Most recent</div>
                )}
              </div>
            </div>
            <Button
              size="sm"
              variant="secondary"
              loading={busy === point.snapshot_id}
              disabled={busy !== null}
              onClick={() => void restore(point.snapshot_id)}
            >
              <RotateCcw className="h-3.5 w-3.5" />
              Restore
            </Button>
          </li>
        ))}
      </ul>
    </div>
  );
}

function FolderSheet({
  folder,
  onClose,
  onChanged,
  onDelete,
}: {
  folder: Folder;
  onClose: () => void;
  onChanged: () => void;
  onDelete: (folder: Folder) => void;
}) {
  const [pending, setPending] = useState<{
    enabled: boolean;
    schedule: string;
  } | null>(null);
  const [custom, setCustom] = useState(
    !BACKUP_PRESETS.some((p) => p.cron === folder.backup?.schedule),
  );
  const [draft, setDraft] = useState(
    folder.backup?.schedule ?? DEFAULT_FOLDER_SCHEDULE,
  );
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);

  const enabled = pending?.enabled ?? folder.backup?.enabled ?? false;
  const schedule =
    pending?.schedule ?? folder.backup?.schedule ?? DEFAULT_FOLDER_SCHEDULE;
  const ownBackups = !(isDataFolder(folder) && folder.used_by.length > 0);

  async function save(next: { enabled: boolean; schedule: string }) {
    setPending(next);
    setNote(null);
    try {
      await api.put(
        `/api/folders/${encodeURIComponent(folder.name)}/backup`,
        next,
      );
      setNote({ ok: true, text: "Saved" });
      onChanged();
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
    setBusy(true);
    setNote(null);
    try {
      await api.post(`/api/folders/${encodeURIComponent(folder.name)}/run-now`);
      setNote({ ok: true, text: "Backup started" });
      onChanged();
    } catch (e) {
      setNote({
        ok: false,
        text: e instanceof Error ? e.message : "Could not start a backup.",
      });
    } finally {
      setBusy(false);
    }
  }

  const path = `/data/${folder.name}`;

  return (
    <Sheet
      open={folder !== null}
      onClose={onClose}
      title={folder.title}
      subtitle={folder.ready ? undefined : "Being created…"}
    >
      {(isRestoring(folder) || folder.restore_error) && (
        <div className="mb-4">
          {folder.restore_error ? (
            <Banner tone="error" title="That restore did not finish">
              {folder.restore_error}
            </Banner>
          ) : (
            <Banner tone="info" title="Restoring from a backup">
              Its files are being copied back. This can take a while.
            </Banner>
          )}
        </div>
      )}

      <Section title="This folder">
        <Row label="Used by" detail={usedByLine(folder)} />
        <Row label="Room for" detail={folderSize(folder.size)} />
        <ValueRow label="Location" value={path} copy />
      </Section>

      {ownBackups && (
        <Section
          title="Backups"
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
            label="Save this folder automatically"
            detail="Encrypted and kept outside this machine"
            trailing={
              <Switch
                checked={enabled}
                onChange={(v) => void save({ enabled: v, schedule })}
                label="Save this folder automatically"
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
                    Minute, hour, day, month, weekday, in the server&rsquo;s
                    time. <code className="font-mono">0 3 * * *</code> is every
                    day at 03:00.
                  </p>
                </div>
              )}
            </Row>
          )}
          <Row
            label="Back up now"
            detail="Copies it right away"
            trailing={
              <RowAction onClick={() => void backupNow()} disabled={busy}>
                {busy ? "Starting…" : "Back up now"}
              </RowAction>
            }
          />
        </Section>
      )}

      {ownBackups && (
        <Section title="Restore a copy">
          <div className="px-5 py-4">
            <p className="mb-3 text-sm text-fg-muted">
              Restoring makes a new folder beside this one. Nothing here
              changes.
            </p>
            <FolderRestorePoints name={folder.name} onRestored={onChanged} />
          </div>
        </Section>
      )}

      <div className="mt-6 flex justify-end">
        <Button
          variant="secondary"
          onClick={() => onDelete(folder)}
          className="text-danger"
        >
          <Trash2 className="h-4 w-4" />
          Delete folder
        </Button>
      </div>
    </Sheet>
  );
}

export function FilesPage() {
  const folders = useApi<Folder[]>("folders", "/api/folders", {
    pollMs: 15_000,
  });
  const [creating, setCreating] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [removing, setRemoving] = useState<Folder | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function remove(folder: Folder) {
    setBusy(true);
    setError(null);
    try {
      await api.del(`/api/folders/${encodeURIComponent(folder.name)}`);
      await folders.refresh();
      setRemoving(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  const list = folders.data ?? [];
  const active = list.find((f) => f.name === selected) ?? null;

  return (
    <Page
      title="Your files"
      subtitle="Folders your apps can share: one Movies folder for your downloader and your media server, one Photos folder for every photo app."
      action={
        <Button onClick={() => setCreating(true)}>
          <Plus className="h-4 w-4" />
          New folder
        </Button>
      }
    >
      <Banner tone="info" title="Your apps' own data is saved with the app">
        Shared folders are only saved if you turn it on, or once the app that
        made them is gone. Open a folder to set that up.
      </Banner>

      {folders.error && !folders.data ? (
        <ServiceTrouble onRetry={() => void folders.refresh()} />
      ) : !folders.data ? (
        <Skeleton className="mt-8 h-32" />
      ) : list.length === 0 ? (
        <EmptyState
          icon={<FolderOpen className="h-6 w-6" />}
          title="No folders yet"
          body="Create one here, or pick “Create a new folder” while installing an app."
          action={
            <Button onClick={() => setCreating(true)}>
              <Plus className="h-4 w-4" />
              New folder
            </Button>
          }
        />
      ) : (
        <Section title="Folders">
          {list.map((f) => {
            const label = folderBackupLabel(f);
            return (
              <Row
                key={f.name}
                onClick={() => setSelected(f.name)}
                label={f.ready ? f.title : `${f.title} — being created`}
                detail={`${usedByLine(f)} · room for ${folderSize(f.size)} · /data/${f.name}${label ? ` · ${label}` : ""}`}
              />
            );
          })}
        </Section>
      )}

      <NewFolderSheet
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={() => void folders.refresh()}
      />

      {active && (
        <FolderSheet
          key={active.name}
          folder={active}
          onClose={() => setSelected(null)}
          onChanged={() => void folders.refresh()}
          onDelete={(f) => {
            setSelected(null);
            setError(null);
            setRemoving(f);
          }}
        />
      )}

      <ConfirmDialog
        open={removing !== null}
        onClose={() => setRemoving(null)}
        onConfirm={() => removing && void remove(removing)}
        title={`Delete ${removing?.title ?? "this folder"}?`}
        body={
          <>
            {removing && removing.used_by.length > 0 ? (
              <p>
                {usedByLine(removing)}. Change their settings first so they stop
                using it.
              </p>
            ) : (
              <p>
                Every file in it is deleted for good, along with its backups.
              </p>
            )}
            {error && <p className="mt-3 text-danger">{error}</p>}
          </>
        }
        confirmLabel="Delete folder and files"
        destructive
        busy={busy}
      />
    </Page>
  );
}
