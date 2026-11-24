import { afterEach, describe, expect, it } from "vitest";
import { easeOut, mergeRows, prefersReducedMotion, rollFormat } from "./motion";

const key = (s: string) => s;
const rows = (keys: string[], leaving: string[] = []) =>
  keys.map((k) => ({ key: k, item: k, leaving: leaving.includes(k) }));

describe("mergeRows", () => {
  it("keeps a removed item in place, marked as leaving", () => {
    expect(mergeRows(rows(["a", "b", "c"]), ["a", "c"], key)).toEqual(
      rows(["a", "b", "c"], ["b"]),
    );
  });

  it("adds a new item where the data puts it", () => {
    expect(mergeRows(rows(["a", "c"]), ["a", "b", "c"], key)).toEqual(
      rows(["a", "b", "c"]),
    );
  });

  it("brings a leaving item back if it reappears", () => {
    expect(mergeRows(rows(["a", "b"], ["b"]), ["a", "b"], key)).toEqual(
      rows(["a", "b"]),
    );
  });

  it("follows the new order of the items that stay", () => {
    expect(mergeRows(rows(["a", "b"]), ["b", "a"], key)).toEqual(
      rows(["b", "a"]),
    );
  });

  it("starts with every item present and none leaving", () => {
    expect(mergeRows([], ["a", "b"], key)).toEqual(rows(["a", "b"]));
  });
});

describe("rolling numbers", () => {
  it("counts whole numbers in whole steps", () => {
    expect(rollFormat(12)(7.6)).toBe("8");
  });

  it("keeps the precision of the value it lands on", () => {
    expect(rollFormat(42.5)(12.345)).toBe("12.3");
  });

  it("starts at the old value and lands exactly on the new one", () => {
    expect(easeOut(0)).toBe(0);
    expect(easeOut(1)).toBe(1);
    expect(easeOut(2)).toBe(1);
  });
});

describe("prefersReducedMotion", () => {
  const setMatchMedia = (value: unknown) =>
    Object.defineProperty(window, "matchMedia", {
      value,
      configurable: true,
      writable: true,
    });

  afterEach(() => {
    Reflect.deleteProperty(window, "matchMedia");
  });

  it("follows the browser's reduced-motion setting", () => {
    for (const matches of [true, false]) {
      setMatchMedia((media: string) => ({ matches, media }));
      expect(prefersReducedMotion()).toBe(matches);
    }
  });

  it("asks for the reduced-motion query, not some other one", () => {
    const asked: string[] = [];
    setMatchMedia((media: string) => {
      asked.push(media);
      return { matches: false, media };
    });
    prefersReducedMotion();
    expect(asked).toEqual(["(prefers-reduced-motion: reduce)"]);
  });

  it("asks for reduced motion where the browser cannot say", () => {
    setMatchMedia(() => {
      throw new Error("matchMedia is not supported");
    });
    expect(prefersReducedMotion()).toBe(true);
  });
});
