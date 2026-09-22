import { describe, expect, it } from "vitest";
import {
  HEAL_PROBLEM_LABELS,
  HEAL_STEP_LABELS,
  healedFrom,
  type HealStatus,
  type MachineReset,
} from "./heal";

// FORCE HEAL wipes machines. Everything in this file is what the person about
// to authorise that reads, so a missing label is not a cosmetic bug — it is a
// blank where the consequence should be.

const reset = (phase: MachineReset["phase"]): MachineReset => ({
  heal_id: "h1",
  driver: "node1",
  phase,
  error: null,
});

function status(machines: HealStatus["survey"]["machines"]): HealStatus {
  return {
    survey: {
      me: "node2",
      machines,
      unreadable: [],
      ceph_quorum: true,
      kubernetes: true,
      uptime_secs: 100,
      lost_groups: 0,
    },
    problems: [],
    refusal: null,
    plan: { keep_machines: [], remove_machines: [] },
    heal: null,
  };
}

const machine = (
  over: Partial<HealStatus["survey"]["machines"][number]> = {},
) => ({
  label: "node2",
  name: "node2",
  addr: "fd00:cafe::2",
  this_machine: false,
  answers: true,
  reset: null as MachineReset | null,
  ...over,
});

describe("heal labels", () => {
  it("has plain-language wording for every step", () => {
    // Record<HealStep, string> makes a MISSING key a type error, which is why
    // the labels are typed that way. What the type cannot catch is an empty or
    // machine-shaped one, and that is what this asserts.
    for (const [step, label] of Object.entries(HEAL_STEP_LABELS)) {
      expect(label.length, step).toBeGreaterThan(10);
      expect(label, step).not.toMatch(/ceph|k3s|systemd|etcd|osd/i);
    }
  });

  it("has plain-language wording for every problem", () => {
    for (const [problem, label] of Object.entries(HEAL_PROBLEM_LABELS)) {
      expect(label.length, problem).toBeGreaterThan(10);
      // "Storage has lost its quorum" is allowed to say quorum; naming the
      // daemon is not. Someone reading this has a laptop, not a cluster.
      expect(label, problem).not.toMatch(/ceph|k3s|kubernetes|etcd|osd|rbd/i);
    }
  });
});

describe("healedFrom", () => {
  it("reports a heal another machine is running on this one", () => {
    // This is what makes a machine show "another machine is resetting me"
    // instead of an ordinary dashboard while it is about to be wiped.
    for (const phase of ["preparing", "prepared", "armed"] as const) {
      const found = healedFrom(
        status([machine({ this_machine: true, reset: reset(phase) })]),
      );
      expect(found?.phase, phase).toBe(phase);
    }
  });

  it("stops reporting once the reset is over", () => {
    // failed / restarted / undone are all finished states. Continuing to show
    // the banner would leave a machine looking permanently doomed.
    for (const phase of ["failed", "restarted", "undone"] as const) {
      expect(
        healedFrom(
          status([machine({ this_machine: true, reset: reset(phase) })]),
        ),
        phase,
      ).toBeNull();
    }
  });

  it("ignores a reset that belongs to a different machine", () => {
    expect(
      healedFrom(
        status([machine({ this_machine: false, reset: reset("armed") })]),
      ),
    ).toBeNull();
  });

  it("copes with this machine not being in the survey at all", () => {
    expect(healedFrom(status([]))).toBeNull();
  });
});
