import { describe, expect, it } from "vitest";
import { formatBytes, generateSecret } from "./format";

describe("formatBytes", () => {
  // The whole point of this module is that machine units never reach the
  // screen, so the cases that matter are the ones that would otherwise render
  // as something a person cannot act on.
  it("never renders a non-number as a size", () => {
    // A disk whose size could not be read must not become "NaN B" on the
    // Storage page — that reads as a broken app rather than a missing fact.
    expect(formatBytes(Number.NaN)).toBe("0 B");
    expect(formatBytes(Number.POSITIVE_INFINITY)).toBe("0 B");
    expect(formatBytes(-1)).toBe("0 B");
    expect(formatBytes(0)).toBe("0 B");
  });

  it("uses decimal units, because that is what disks are sold in", () => {
    // 1000, not 1024. A 1 TB disk must read as 1 TB, not 931 GB — the number
    // on the label is the only one the owner can check against.
    expect(formatBytes(1000)).toBe("1 KB");
    expect(formatBytes(1_000_000)).toBe("1 MB");
    expect(formatBytes(1_000_000_000)).toBe("1 GB");
    expect(formatBytes(1_400_000_000_000)).toBe("1.4 TB");
  });

  it("does not show a trailing .0, which reads as spurious precision", () => {
    expect(formatBytes(2_000_000_000)).toBe("2 GB");
  });

  it("drops the decimal once the number is big enough not to need it", () => {
    // At three digits a tenth is noise: "312 GB", not "312.4 GB".
    expect(formatBytes(312_400_000_000)).toBe("312 GB");
  });

  it("never shows a fraction of a byte", () => {
    expect(formatBytes(999)).toBe("999 B");
    expect(formatBytes(1)).toBe("1 B");
  });

  it("honours a caller that wants more precision", () => {
    expect(formatBytes(1_234_000_000, 2)).toBe("1.23 GB");
  });

  it("clamps at the largest unit it knows rather than running off the end", () => {
    // Beyond PB the unit array has nothing left; the index must not walk past
    // it and render "undefined".
    expect(formatBytes(1e24)).toMatch(/ PB$/);
  });
});

describe("generateSecret", () => {
  it("is the length asked for", () => {
    expect(generateSecret(24)).toHaveLength(24);
    expect(generateSecret(8)).toHaveLength(8);
    expect(generateSecret()).toHaveLength(24);
  });

  it("excludes every character that is ambiguous when retyped", () => {
    // These get copied off a screen onto a phone far more often than anyone
    // plans for, and 0/O or 1/l/I is where that goes wrong. Sampled across
    // many draws because the alphabet is picked at random per character.
    const drawn = Array.from({ length: 200 }, () => generateSecret(32)).join(
      "",
    );
    for (const ambiguous of ["0", "O", "1", "l", "I"]) {
      expect(drawn).not.toContain(ambiguous);
    }
  });

  it("uses more than one character, i.e. is actually random", () => {
    expect(new Set(generateSecret(64)).size).toBeGreaterThan(8);
  });
});
