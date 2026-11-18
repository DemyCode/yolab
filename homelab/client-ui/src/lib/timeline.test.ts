import { describe, expect, it } from "vitest";
import { NEAREST_HOURS, ageHours, frameRange, shortAge } from "./timeline";

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
