import { keptReason, type DataVerdict } from "@/lib/removeData";
import { useApi } from "@/lib/useResource";

export function RemoveDataChoice({
  instance,
  open,
  deleteData,
  onChange,
}: {
  instance: string;
  open: boolean;
  deleteData: boolean;
  onChange: (next: boolean) => void;
}) {
  const verdict = useApi<DataVerdict>(
    open ? `app-data-${instance}` : null,
    `/api/apps/${encodeURIComponent(instance)}/data`,
    { persist: false },
  );
  const data = verdict.data;

  return (
    <div className="mt-4 space-y-3">
      <label className="flex cursor-pointer items-start gap-2">
        <input
          type="checkbox"
          checked={deleteData}
          onChange={() => onChange(!deleteData)}
          className="mt-0.5 h-4 w-4 rounded border-border-strong bg-surface-2 accent-primary"
        />
        <span className="text-sm text-fg">Also delete its data</span>
      </label>

      {verdict.loading && !data && (
        <p className="text-xs text-fg-muted">Looking at its folders…</p>
      )}
      {verdict.error && !data && (
        <p className="text-xs text-danger">
          Its folders could not be read: {verdict.error}
        </p>
      )}

      {data && deleteData && data.delete.length > 0 && (
        <div>
          <p className="text-xs text-fg-muted">These folders are deleted:</p>
          <ul className="mt-1 space-y-0.5 font-mono text-xs text-fg">
            {data.delete.map((folder) => (
              <li key={folder}>{folder}</li>
            ))}
          </ul>
        </div>
      )}

      {data && deleteData && data.kept.length > 0 && (
        <div>
          <p className="text-xs text-fg-muted">
            These stay, because another service uses them:
          </p>
          <ul className="mt-1 space-y-0.5 font-mono text-xs text-fg">
            {data.kept.map((kept) => (
              <li key={kept.folder}>{keptReason(kept)}</li>
            ))}
          </ul>
        </div>
      )}

      {!deleteData && (
        <p className="text-xs text-fg-muted">
          Its folders stay in your file tree. Installing it again under the same
          name picks them up.
        </p>
      )}
    </div>
  );
}
