import { describe, expect, it } from "vitest";
import {
  isVisible,
  jobOf,
  movementCopy,
  needsAttentionEverywhere,
  overallProgress,
  percent,
  timeLeft,
} from "./movement";
import type { Movement, MovementJob } from "@/types/storage";

function job(over: Partial<MovementJob> = {}): MovementJob {
  return {
    kind: "move",
    remaining_bytes: 0,
    to_move_bytes: 0,
    moved_bytes: 0,
    progress: null,
    eta_secs: null,
    ...over,
  };
}

function movement(over: Partial<Movement> = {}): Movement {
  return {
    state: "working",
    jobs: [job()],
    draining: [],
    waiting_for: [],
    copies: null,
    eta_secs: null,
    inactive_pgs: 0,
    total_pgs: 81,
    ...over,
  };
}

const easystore = [{ node: "node2", name: "easystore 2647" }];

describe("movementCopy", () => {
  it("names the disk being emptied and says the files are safe", () => {
    const copy = movementCopy(movement({ draining: easystore }));
    expect(copy?.headline).toBe(
      "Moving your files off the easystore 2647 disk on node2",
    );
    expect(copy?.tone).toBe("calm");
    expect(copy?.safety).toMatch(/safe/);
  });

  it("calls a raised copy count a gain, not a loss", () => {
    const copy = movementCopy(
      movement({ jobs: [job({ kind: "add_copies" })], copies: 2 }),
    );
    expect(copy?.headline).toBe("Adding a second copy of every file");
    expect(copy?.tone).toBe("calm");
    expect(copy?.jobs[0].note).toMatch(/You chose 2 copies/);
    expect(copy?.safety).not.toMatch(/fewer copies/);
  });

  it("shows a drain and a second copy as two lines under one headline", () => {
    const copy = movementCopy(
      movement({
        jobs: [job({ kind: "move" }), job({ kind: "add_copies" })],
        draining: easystore,
        copies: 2,
      }),
    );
    expect(copy?.headline).toBe("Reorganising your files");
    expect(copy?.jobs.map((j) => j.label)).toEqual([
      "Moving files off the easystore 2647 disk on node2",
      "Adding a second copy of every file",
    ]);
    expect(copy?.safety).toMatch(/Don't unplug/);
  });

  it("is only worried when copies are rebuilt after a loss", () => {
    const copy = movementCopy(
      movement({ jobs: [job({ kind: "rebuild" })], waiting_for: ["node1"] }),
    );
    expect(copy?.headline).toBe("Rebuilding copies that were on node1");
    expect(copy?.tone).toBe("warning");
    expect(copy?.safety).toMatch(/fewer copies than you chose/);
  });

  it("says what it is waiting for when files can't be reached", () => {
    const copy = movementCopy(
      movement({ state: "unavailable", waiting_for: ["node1", "node2"] }),
    );
    expect(copy?.headline).toBe("Waiting for node1 and node2");
    expect(copy?.tone).toBe("error");
    expect(copy?.jobs).toEqual([]);
  });

  it("says nothing when everything is in place or unknown", () => {
    expect(movementCopy(movement({ state: "settled" }))).toBeNull();
    expect(isVisible(movement({ state: "settled" }))).toBe(false);
    expect(isVisible(movement({ state: "unknown" }))).toBe(false);
    expect(isVisible(undefined)).toBe(false);
  });
});

describe("overallProgress", () => {
  it("weighs each job by how much it has to move", () => {
    const m = movement({
      jobs: [
        job({ to_move_bytes: 100, moved_bytes: 50, progress: 0.5 }),
        job({
          kind: "add_copies",
          to_move_bytes: 300,
          moved_bytes: 0,
          progress: 0,
        }),
      ],
    });
    expect(overallProgress(m)).toBeCloseTo(0.125);
  });

  it("has nothing to show before any job was measured", () => {
    expect(
      overallProgress(movement({ jobs: [job({ to_move_bytes: 5 })] })),
    ).toBeNull();
    expect(overallProgress(movement({ jobs: [] }))).toBeNull();
  });

  it("finds one job by its kind", () => {
    const m = movement({ jobs: [job({ kind: "add_copies" })] });
    expect(jobOf(m, "add_copies")?.kind).toBe("add_copies");
    expect(jobOf(m, "move")).toBeUndefined();
  });
});

describe("timeLeft", () => {
  it("does not promise a time before there is a rate", () => {
    expect(timeLeft(null)).toBe("Estimating time left…");
  });

  it("rounds to rough, readable amounts", () => {
    expect(timeLeft(30)).toBe("less than a minute left");
    expect(timeLeft(4 * 60)).toBe("about 4 min left");
    expect(timeLeft(23 * 60)).toBe("about 25 min left");
    expect(timeLeft(60 * 60)).toBe("about an hour left");
    expect(timeLeft(5 * 3600)).toBe("about 5 hours left");
    expect(timeLeft(4 * 86400)).toBe("about 4 days left");
  });
});

describe("percent and reach", () => {
  it("keeps the bar between empty and full", () => {
    expect(percent(null)).toBe(0);
    expect(percent(0.37)).toBeCloseTo(37);
    expect(percent(1.4)).toBe(100);
  });

  it("only interrupts the home page when something can't be used or finished", () => {
    expect(needsAttentionEverywhere(movement())).toBe(false);
    expect(needsAttentionEverywhere(movement({ state: "unavailable" }))).toBe(
      true,
    );
    expect(needsAttentionEverywhere(movement({ state: "no_room" }))).toBe(true);
  });
});
