import { useApi } from "@/lib/useResource";

export interface MachineReset {
  heal_id: string;
  driver: string;
  phase: "preparing" | "prepared" | "failed" | "armed" | "restarted" | "undone";
  error: string | null;
}

export interface HealStatus {
  survey: {
    me: string;
    machines: {
      label: string;
      name: string | null;
      addr: string;
      this_machine: boolean;
      answers: boolean;
      reset: MachineReset | null;
    }[];
    unreadable: string[];
    ceph_quorum: boolean;
    kubernetes: boolean;
    uptime_secs: number;
    lost_groups: number | null;
  };
  problems: HealProblem[];
  refusal: string | null;
  plan: {
    keep_machines: string[];
    remove_machines: string[];
  };
  heal: Heal | null;
}

export type HealProblem =
  "machines_gone" | "ceph_no_quorum" | "kubernetes_down" | "data_unreachable";

export type HealStep = "prepare" | "arm" | "restart" | "rebuild" | "undo";

export interface Heal {
  id: string;
  driver: string;
  running: boolean;
  failed: string | null;
  started_at: number;
  finished_at: number | null;
  step: HealStep;
  steps: HealStep[];
  members: string[];
  removed_machines: string[];
  waiting: string | null;
}

export const HEAL_STEP_LABELS: Record<HealStep, string> = {
  prepare: "Preparing every machine for the new cluster",
  arm: "Setting every machine to start fresh",
  restart: "Restarting every machine",
  rebuild: "Starting the new cluster",
  undo: "Putting every machine back as it was",
};

export const HEAL_PROBLEM_LABELS: Record<HealProblem, string> = {
  machines_gone: "A machine does not answer",
  ceph_no_quorum: "Storage has lost its quorum",
  kubernetes_down: "The cluster is not answering",
  data_unreachable: "Some of your files have no reachable copy",
};

export function healedFrom(status: HealStatus): MachineReset | null {
  const me = status.survey.machines.find((m) => m.this_machine);
  const reset = me?.reset;
  return reset && ["preparing", "prepared", "armed"].includes(reset.phase)
    ? reset
    : null;
}

export function useHealStatus(pollMs: number) {
  return useApi<HealStatus>("heal", "/api/heal", { pollMs });
}

export const SHOW_FINISHED_FOR_SECS = 24 * 3600;

export function recentlyFinished(heal: Heal | null, nowSecs: number): boolean {
  return (
    heal !== null &&
    !heal.running &&
    heal.finished_at !== null &&
    nowSecs - heal.finished_at < SHOW_FINISHED_FOR_SECS
  );
}

export type BannerTone = "error" | "warning" | "info" | "success";

export function healBannerTone(
  status: HealStatus | undefined,
  nowSecs: number,
): BannerTone | null {
  if (!status) return null;
  if (status.heal?.running || healedFrom(status)) return "info";
  if (status.problems.length > 0) return "error";
  if (!recentlyFinished(status.heal, nowSecs)) return null;
  return status.heal?.failed ? "warning" : "info";
}

const RANK: Record<BannerTone, number> = {
  error: 0,
  warning: 1,
  info: 2,
  success: 3,
};

export function oneBanner<K extends string>(
  candidates: [K, BannerTone | null][],
): K | null {
  let best: [K, BannerTone] | null = null;
  for (const [key, tone] of candidates) {
    if (tone === null) continue;
    if (best === null || RANK[tone] < RANK[best[1]]) best = [key, tone];
  }
  return best?.[0] ?? null;
}
