import {
  folderName,
  sharedWith,
  usedBy,
  type DataVerdict,
} from "@/lib/removeData";
import { useApi } from "@/lib/useResource";

function Folders({ label, folders }: { label: string; folders: string[] }) {
  if (folders.length === 0) return null;
  return (
    <div>
      <p className="text-xs text-fg-muted">{label}</p>
      <ul className="mt-1 space-y-0.5 font-mono text-xs text-fg">
        {folders.map((folder) => (
          <li key={folder}>{folder}</li>
        ))}
      </ul>
    </div>
  );
}

export function RemoveDataChoice({
  instance,
  open,
  deleteShared,
  onChange,
}: {
  instance: string;
  open: boolean;
  deleteShared: boolean;
  onChange: (next: boolean) => void;
}) {
  const verdict = useApi<DataVerdict>(
    open ? `app-data-${instance}` : null,
    `/api/apps/${encodeURIComponent(instance)}/data`,
    { persist: false },
  );
  const data = verdict.data;
  const nothing =
    data &&
    data.delete.length === 0 &&
    data.shared.length === 0 &&
    data.kept.length === 0;

  return (
    <div className="mt-4 space-y-3">
      {verdict.loading && !data && (
        <p className="text-xs text-fg-muted">Looking at its folders…</p>
      )}
      {verdict.error && !data && (
        <p className="text-xs text-danger">
          Its folders could not be read: {verdict.error}. Removing it still
          deletes the data no other service uses.
        </p>
      )}
      {nothing && (
        <p className="text-xs text-fg-muted">
          It keeps no data in your file tree.
        </p>
      )}

      {data && (
        <Folders
          label="This data will be deleted:"
          folders={data.delete.map(folderName)}
        />
      )}

      {data && data.shared.length > 0 && (
        <div>
          <p className="text-xs text-fg-muted">
            This data is still read and written by{" "}
            {sharedWith(data.shared).join(", ")}. Delete it too?
          </p>
          <ul className="mt-1 space-y-0.5 font-mono text-xs text-fg">
            {data.shared.map((s) => (
              <li key={s.folder}>{folderName(s.folder)}</li>
            ))}
          </ul>
          <div className="mt-2 flex gap-4">
            {[
              { value: true, label: "Yes" },
              { value: false, label: "No" },
            ].map((choice) => (
              <label
                key={choice.label}
                className="flex cursor-pointer items-center gap-2"
              >
                <input
                  type="radio"
                  name={`delete-shared-${instance}`}
                  checked={deleteShared === choice.value}
                  onChange={() => onChange(choice.value)}
                  className="h-4 w-4 border-border-strong bg-surface-2 accent-primary"
                />
                <span className="text-sm text-fg">{choice.label}</span>
              </label>
            ))}
          </div>
        </div>
      )}

      {data && (
        <Folders
          label="Kept, because another service mounts it:"
          folders={data.kept.map(usedBy)}
        />
      )}
    </div>
  );
}
