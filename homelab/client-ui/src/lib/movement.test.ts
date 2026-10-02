import { describe, expect, it } from "vitest";
import {
  isVisible,
  movementCopy,
  needsAttentionEverywhere,
  percent,
  timeLeft,
} from "./movement";
import type { Movement } from "@/types/storage";

function movement(over: Partial<Movement> = {}): Movement {
  return {
    state: "moving",
    draining: [],
    waiting_for: [],
    remaining_bytes: 0,
    moved_bytes: 0,
    to_move_bytes: 0,
    progress: null,
    eta_secs: null,
    inactive_pgs: 0,
    total_pgs: 81,
    ...over,
  };
}

describe("movementCopy", () => {
  it("names the disk being emptied and says the files are safe", () => {
    const copy = movementCopy(
      movement({ draining: [{ node: "node2", name: "easystore 2647" }] }),
    );
    expect(copy?.headline).toBe(
      "Moving your files off the easystore 2647 disk on node2",
    );
    expect(copy?.tone).toBe("calm");
    expect(copy?.safety).toMatch(/safe/);
  });

  it("falls back to a general sentence when no disk is being removed", () => {
    expect(movementCopy(movement())?.headline).toBe(
      "Rearranging your files across your disks",
    );
  });

  it("says what it is waiting for when files can't be reached", () => {
    const copy = movementCopy(
      movement({ state: "unavailable", waiting_for: ["node1", "node2"] }),
    );
    expect(copy?.headline).toBe("Waiting for node1 and node2");
    expect(copy?.tone).toBe("error");
    expect(copy?.showsProgress).toBe(false);
  });

  it("warns against unplugging while copies are rebuilt", () => {
    const copy = movementCopy(movement({ state: "rebuilding" }));
    expect(copy?.tone).toBe("warning");
    expect(copy?.safety).toMatch(/unplug/);
  });

  it("says nothing when everything is in place or unknown", () => {
    expect(movementCopy(movement({ state: "settled" }))).toBeNull();
    expect(isVisible(movement({ state: "settled" }))).toBe(false);
    expect(isVisible(movement({ state: "unknown" }))).toBe(false);
    expect(isVisible(undefined)).toBe(false);
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
    expect(needsAttentionEverywhere(movement({ state: "rebuilding" }))).toBe(
      false,
    );
    expect(needsAttentionEverywhere(movement({ state: "unavailable" }))).toBe(
      true,
    );
    expect(needsAttentionEverywhere(movement({ state: "no_room" }))).toBe(true);
  });
});
