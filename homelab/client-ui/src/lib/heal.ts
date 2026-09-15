import { useApi } from "@/lib/useResource";

/** `GET /api/heal` — see heal.rs `status_json`. */
export interface HealStatus {
  survey: {
    me: string;
    machines: {
      name: string;
      addr: string;
      this_machine: boolean;
      answers: boolean;
    }[];
    ceph_quorum: boolean;
    kubernetes: boolean;
    uptime_secs: number;
    /** Known only while Ceph has a quorum. */
    down_osds: number[] | null;
    lost_groups: number | null;
  };
  problems: HealProblem[];
  /** Why a heal cannot start from this machine right now, if it cannot. */
  refusal: string | null;
  plan: {
    remove_machines: string[];
    restart_machines: string[];
    reset_kubernetes: boolean;
  };
  heal: Heal | null;
}

export type HealProblem =
  "machines_gone" | "ceph_no_quorum" | "kubernetes_down" | "data_unreachable";

export type HealStep = "claim" | "consensus" | "wipe" | "restart" | "finish";

export interface Heal {
  id: string;
  driver: string;
  running: boolean;
  started_at: number;
  finished_at: number | null;
  step: HealStep;
  steps: HealStep[];
  removed_machines: string[];
  restarted_machines: string[];
  reset_kubernetes: boolean;
  /** What the current step is waiting for, or why it failed last. */
  waiting: string | null;
}

export const HEAL_STEP_LABELS: Record<HealStep, string> = {
  claim: "Making sure no other machine is healing",
  consensus: "Getting the remaining machines to agree again",
  wipe: "Deleting everything that is stored",
  restart: "Restarting the machines",
  finish: "Removing the old apps and the missing machines",
};

export const HEAL_PROBLEM_LABELS: Record<HealProblem, string> = {
  machines_gone: "A machine does not answer",
  ceph_no_quorum: "Storage has lost its quorum",
  kubernetes_down: "The cluster is not answering",
  data_unreachable: "Some of your files have no reachable copy",
};

export function useHealStatus(pollMs: number) {
  return useApi<HealStatus>("heal", "/api/heal", { pollMs });
}
