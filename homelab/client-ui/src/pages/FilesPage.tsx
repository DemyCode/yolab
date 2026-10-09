import { useState } from "react";
import { FolderOpen, Plus, Trash2 } from "lucide-react";
import { Page } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import {
  Banner,
  EmptyState,
  ServiceTrouble,
  Skeleton,
} from "@/components/ui/feedback";
import { Field, Input, Select } from "@/components/ui/input";
import { IconButton, Row, Section } from "@/components/ui/list";
import { ConfirmDialog, Sheet } from "@/components/ui/sheet";
import { api } from "@/lib/api";
import {
  DEFAULT_SIZE_GIB,
  SIZE_CHOICES_GIB,
  folderSize,
  usedByLine,
  type Folder,
} from "@/lib/folders";
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

export function FilesPage() {
  const folders = useApi<Folder[]>("folders", "/api/folders", {
    pollMs: 15_000,
  });
  const [creating, setCreating] = useState(false);
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
      <Banner tone="info" title="Folders are not backed up yet">
        Backups cover each app's own files. Keep a copy of anything
        irreplaceable you put in a folder somewhere else for now.
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
          {list.map((f) => (
            <Row
              key={f.name}
              label={f.ready ? f.title : `${f.title} — being created`}
              detail={`${usedByLine(f)} · room for ${folderSize(f.size)} · /data/${f.name}`}
              trailing={
                <IconButton
                  label={`Delete ${f.title}`}
                  onClick={() => {
                    setError(null);
                    setRemoving(f);
                  }}
                >
                  <Trash2 className="h-4 w-4" />
                </IconButton>
              }
            />
          ))}
        </Section>
      )}

      <NewFolderSheet
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={() => void folders.refresh()}
      />

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
                Every file in it is deleted for good. Backups of your apps do
                not include it.
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
