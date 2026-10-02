import { describe, expect, it } from "vitest";
import { frameSavedAt, rememberedMeta, shouldShowFrame } from "./useResource";

describe("useResource frame ordering", () => {
  it("labels a remembered value as cached, with its real age", () => {
    expect(rememberedMeta({ data: 1, savedAt: 1_000 }, 4_000)).toEqual({
      state: "stale",
      ageMs: 3_000,
      ttlMs: 0,
    });
  });

  it("dates a frame by its age, not by when it arrived", () => {
    expect(
      frameSavedAt({ state: "stale", ageMs: 3_600_000, ttlMs: 15_000 }, 4_000_000),
    ).toBe(400_000);
  });

  it("skips a cached frame older than what is already on screen", () => {
    const old = { state: "stale" as const, ageMs: 60_000, ttlMs: 15_000 };
    expect(shouldShowFrame(old, 1_000, 5_000)).toBe(false);
  });

  it("shows a cached frame newer than what is on screen", () => {
    const recent = { state: "hit" as const, ageMs: 1_000, ttlMs: 15_000 };
    expect(shouldShowFrame(recent, 9_000, 5_000)).toBe(true);
  });

  it("always shows the real answer", () => {
    for (const state of ["fresh", "miss"] as const) {
      expect(shouldShowFrame({ state, ageMs: 0, ttlMs: 15_000 }, 0, 5_000)).toBe(
        true,
      );
    }
    expect(shouldShowFrame(null, 0, 5_000)).toBe(true);
  });
});
