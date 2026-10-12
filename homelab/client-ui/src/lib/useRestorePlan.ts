import { useEffect, useState } from "react";
import { api } from "@/lib/api";
import type { RestorePlan } from "@/lib/restore";

export function useRestorePlan(namespace: string | null, snapshot: string) {
  const key = namespace && snapshot ? `${namespace}|${snapshot}` : null;
  const [read, setRead] = useState<{
    key: string;
    plan: RestorePlan | null;
    error: string | null;
  } | null>(null);
  useEffect(() => {
    if (!namespace || !key) return;
    let cancelled = false;
    void api
      .get<RestorePlan>(
        `/api/backups/apps/${encodeURIComponent(namespace)}/folders?snapshot_id=${encodeURIComponent(snapshot)}`,
      )
      .then((plan) => {
        if (!cancelled) setRead({ key, plan, error: null });
      })
      .catch((e: unknown) => {
        if (!cancelled)
          setRead({
            key,
            plan: null,
            error:
              e instanceof Error ? e.message : "The backup could not be read.",
          });
      });
    return () => {
      cancelled = true;
    };
  }, [namespace, snapshot, key]);
  const current = read && read.key === key ? read : null;
  return { plan: current?.plan ?? null, error: current?.error ?? null };
}

