import { describe, expect, it } from "vitest";
import { easeOut, mergeRows, rollFormat } from "./motion";

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
