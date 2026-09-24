import { useEffect, useState } from "react";
import { api } from "@/lib/api";
import { sortRestorePoints } from "@/lib/backups";
import type { RestorePoint } from "@/lib/backups";

interface Loaded {
  namespace: string;
  points: RestorePoint[];
  error: string | null;
}

export function useRestorePoints(namespace: string | null) {
  const [loaded, setLoaded] = useState<Loaded | null>(null);

  useEffect(() => {
    if (!namespace) return;
    let cancelled = false;
    void api
      .get<{ points?: RestorePoint[] }>(
        `/api/backups/apps/${encodeURIComponent(namespace)}/points`,
      )
      .then((d) => {
        if (cancelled) return;
        setLoaded({
          namespace,
          points: sortRestorePoints(d.points ?? []),
          error: null,
        });
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setLoaded({
          namespace,
          points: [],
          error:
            e instanceof Error ? e.message : "Your backups could not be read.",
        });
      });
    return () => {
      cancelled = true;
    };
  }, [namespace]);

  const fresh = namespace !== null && loaded?.namespace === namespace;
  return {
    points: fresh ? loaded.points : null,
    error: fresh ? loaded.error : null,
  };
}
