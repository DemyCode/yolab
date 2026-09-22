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
