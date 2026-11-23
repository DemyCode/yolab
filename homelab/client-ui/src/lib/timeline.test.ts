import { describe, expect, it } from "vitest";
import {
  AGE_MARKS,
  NEAREST_HOURS,
  ageHours,
  ageMarks,
  frameRange,
  shortAge,
  spread,
  tickLabel,
} from "./timeline";

const NOW = new Date("2026-11-18T12:00:00Z").getTime();

describe("frameRange", () => {
  const ages = [2, 26, 74, 170, 362];

  it("opens on every copy, from the closest to the farthest", () => {
    const [lo, hi] = frameRange(ages, "all", "true");
    expect(lo).toBeLessThan(2);
    expect(lo).toBeGreaterThanOrEqual(0);
    expect(hi).toBeGreaterThan(362);
    expect(hi).toBeLessThan(400);
  });

  it("stays above zero on the even scale, where time is drawn in log", () => {
    const [lo, hi] = frameRange(ages, "all", "even");
    expect(lo).toBeGreaterThan(0);
    expect(lo).toBeLessThan(2);
    expect(hi).toBeGreaterThan(362);
  });

  it("shows exactly the chosen frame when copies fall inside it", () => {
    const [, hi] = frameRange(ages, "week", "true");
    expect(hi).toBeGreaterThanOrEqual(24 * 7);
    expect(hi).toBeLessThan(24 * 7 + 10);
  });

  it("falls back to every copy when none is inside the frame", () => {
    const old = [24 * 400, 24 * 500];
    const [lo, hi] = frameRange(old, "week", "true");
    expect(lo).toBeLessThan(24 * 400);
    expect(hi).toBeGreaterThan(24 * 500);
  });
});

describe("ageHours", () => {
  it("never reaches zero, so a copy taken just now still has a place in log", () => {
    expect(ageHours(new Date(NOW).toISOString(), NOW)).toBe(NEAREST_HOURS);
    expect(ageHours(new Date(NOW - 3_600_000).toISOString(), NOW)).toBe(1);
  });
});

describe("shortAge", () => {
  it("labels the axis in the unit that reads naturally", () => {
    expect(shortAge(0.5)).toBe("30 min");
    expect(shortAge(5)).toBe("5 h");
    expect(shortAge(72)).toBe("3 d");
    expect(shortAge(24 * 120)).toBe("4 mo");
    expect(shortAge(24 * 1000)).toBe("3 y");
  });
});

describe("tickLabel", () => {
  it("leaves a line uPlot chose not to label without a label", () => {
    expect(tickLabel(null)).toBeNull();
    expect(tickLabel(undefined)).toBeNull();
  });

  it("does not read a hidden line as one minute", () => {
    expect([null, 24, null].map(tickLabel)).toEqual([null, "24 h", null]);
  });
});

describe("ageMarks", () => {
  it("marks the human ages inside what is shown", () => {
    expect(ageMarks(2, 24 * 10)).toEqual([3, 6, 12, 24, 48, 72, 168]);
  });

  it("every mark reads as a round age", () => {
    expect(AGE_MARKS.map(shortAge)).toEqual([
      "5 min",
      "15 min",
      "1 h",
      "3 h",
      "6 h",
      "12 h",
      "24 h",
      "2 d",
      "3 d",
      "7 d",
      "14 d",
      "30 d",
      "2 mo",
      "3 mo",
      "6 mo",
      "1 y",
      "2 y",
      "5 y",
    ]);
  });

  it("no two marks read the same", () => {
    const labels = AGE_MARKS.map(shortAge);
    expect(new Set(labels).size).toBe(labels.length);
  });
});

describe("spread", () => {
  it("drops marks that would sit too close to the last one kept", () => {
    const at: Record<number, number> = { 1: 0, 3: 20, 6: 50, 12: 70, 24: 120 };
    expect(spread([1, 3, 6, 12, 24], (h) => at[h], 50)).toEqual([1, 6, 24]);
  });

  it("keeps every mark when there is room", () => {
    expect(spread([1, 2, 3], (h) => h * 100, 50)).toEqual([1, 2, 3]);
  });
});

describe("shortAge in years", () => {
  it("calls a year a year, not twelve months", () => {
    expect(shortAge(24 * 365)).toBe("1 y");
    expect(shortAge(24 * 300)).toBe("10 mo");
  });
});

describe("a timeline of copies minutes apart", () => {
  it("still labels both ends when no round age falls inside", () => {
    expect(ageMarks(0.04, 0.08)).toEqual([0.04, 0.08]);
  });

  it("calls the start of a real-time axis now", () => {
    expect(tickLabel(0)).toBe("now");
  });
});
