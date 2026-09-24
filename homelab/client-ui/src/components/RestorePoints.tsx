import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { History, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/feedback";
import { api } from "@/lib/api";
import { formatDateTime } from "@/lib/format";
import { restorePath } from "@/lib/backups";
import type { RestorePoint } from "@/lib/backups";
import type { AppDefinition } from "@/types/apps";

async function chartOf(
  appId: string | undefined,
  namespace: string,
  snapshotId: string,
): Promise<string> {
  if (appId) return appId;
  const def = await api.get<AppDefinition>(
    `/api/backups/apps/${encodeURIComponent(namespace)}/definition?snapshot_id=${encodeURIComponent(snapshotId)}`,
  );
  if (!def.app_id) throw new Error("That backup does not say which app it is.");
  return def.app_id;
}

export function RestorePointList({
  appId,
  namespace,
  points,
  error,
  emptyHint,
}: {
  appId?: string;
  namespace: string;
  points: RestorePoint[] | null;
  error: string | null;
  emptyHint: string;
}) {
  const navigate = useNavigate();
  const [opening, setOpening] = useState<string | null>(null);
  const [failed, setFailed] = useState<string | null>(null);

  async function open(snapshotId: string) {
    setOpening(snapshotId);
    setFailed(null);
    try {
      const chart = await chartOf(appId, namespace, snapshotId);
      navigate(restorePath(chart, namespace, snapshotId));
    } catch (e) {
      setFailed(
        e instanceof Error ? e.message : "That backup could not be opened.",
      );
    } finally {
      setOpening(null);
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
    return <p className="text-sm text-fg-muted">{emptyHint}</p>;
  }

  return (
    <>
      {failed && <p className="mb-2 text-sm text-danger">{failed}</p>}
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
              loading={opening === point.snapshot_id}
              disabled={opening !== null}
              onClick={() => void open(point.snapshot_id)}
            >
              <RotateCcw className="h-3.5 w-3.5" />
              Restore
            </Button>
          </li>
        ))}
      </ul>
    </>
  );
}
