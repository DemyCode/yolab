import { useApi } from "@/lib/useResource";

/** A machine's part in a heal — see heal/member.rs `ResetView`. */
export interface MachineReset {
  heal_id: string;
  driver: string;
  phase: "building" | "built" | "failed" | "committed" | "restarted" | "undone";
  error: string | null;
}

/** `GET /api/heal` — see heal/mod.rs `status_json`. */
export interface HealStatus {
  survey: {
    me: string;
    machines: {
      /** Its name, or its address when no list knew its name. */
      label: string;
      name: string | null;
      addr: string;
      this_machine: boolean;
      answers: boolean;
      reset: MachineReset | null;
    }[];
    /** Lists of machines that could not be read, and why. */
    unreadable: string[];
    ceph_quorum: boolean;
    kubernetes: boolean;
    uptime_secs: number;
    /** Known only while Ceph has a quorum. */
    lost_groups: number | null;
  };
  problems: HealProblem[];
  /** Why a heal cannot start from this machine right now, if it cannot. */
  refusal: string | null;
  plan: {
    keep_machines: string[];
    remove_machines: string[];
  };
  /** The heal this machine drives, or drove last. */
  heal: Heal | null;
}

export type HealProblem =
  "machines_gone" | "ceph_no_quorum" | "kubernetes_down" | "data_unreachable";

export type HealStep = "build" | "commit" | "restart" | "rebuild" | "undo";

export interface Heal {
  id: string;
  driver: string;
  running: boolean;
  /** Why the heal was abandoned and undone, when it was. */
  failed: string | null;
  started_at: number;
  finished_at: number | null;
  step: HealStep;
  /** The steps of a heal that succeeds, in order. */
  steps: HealStep[];
  members: string[];
  removed_machines: string[];
  /** What the current step is waiting for, or why it failed last. */
  waiting: string | null;
}

export const HEAL_STEP_LABELS: Record<HealStep, string> = {
  build: "Preparing every machine for the new cluster",
  commit: "Switching every machine over",
  restart: "Restarting the machines",
  rebuild: "Starting the new cluster",
  undo: "Putting every machine back as it was",
};

export const HEAL_PROBLEM_LABELS: Record<HealProblem, string> = {
  machines_gone: "A machine does not answer",
  ceph_no_quorum: "Storage has lost its quorum",
  kubernetes_down: "The cluster is not answering",
  data_unreachable: "Some of your files have no reachable copy",
};

/** The heal another machine is carrying out on this one, if any. */
export function healedFrom(status: HealStatus): MachineReset | null {
  const me = status.survey.machines.find((m) => m.this_machine);
  const reset = me?.reset;
  return reset && ["building", "built", "committed"].includes(reset.phase)
    ? reset
    : null;
}

export function useHealStatus(pollMs: number) {
  return useApi<HealStatus>("heal", "/api/heal", { pollMs });
}
