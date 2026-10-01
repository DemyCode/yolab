import { describe, expect, it } from "vitest";
import {
  formatBytes,
  generateSecret,
  relativeTime,
  roughDuration,
} from "./format";

describe("formatBytes", () => {
  it("never renders a non-number as a size", () => {
    expect(formatBytes(Number.NaN)).toBe("0 B");
    expect(formatBytes(Number.POSITIVE_INFINITY)).toBe("0 B");
    expect(formatBytes(-1)).toBe("0 B");
    expect(formatBytes(0)).toBe("0 B");
  });

  it("uses decimal units, because that is what disks are sold in", () => {
    expect(formatBytes(1000)).toBe("1 KB");
    expect(formatBytes(1_000_000)).toBe("1 MB");
    expect(formatBytes(1_000_000_000)).toBe("1 GB");
    expect(formatBytes(1_400_000_000_000)).toBe("1.4 TB");
  });

  it("does not show a trailing .0, which reads as spurious precision", () => {
    expect(formatBytes(2_000_000_000)).toBe("2 GB");
  });

  it("drops the decimal once the number is big enough not to need it", () => {
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

describe("relativeTime", () => {
  const now = new Date("2026-09-28T12:00:00Z");

  it("says just now for the last minute", () => {
    expect(relativeTime("2026-09-28T11:59:30Z", now)).toBe("just now");
  });

  it("counts minutes, then hours", () => {
    expect(relativeTime("2026-09-28T11:59:00Z", now)).toBe("a minute ago");
    expect(relativeTime("2026-09-28T11:45:00Z", now)).toBe("15 minutes ago");
    expect(relativeTime("2026-09-28T11:00:00Z", now)).toBe("an hour ago");
    expect(relativeTime("2026-09-28T07:00:00Z", now)).toBe("5 hours ago");
  });

  it("gives the date for anything older than a day", () => {
    expect(relativeTime("2026-09-20T12:00:00Z", now)).not.toMatch(/ago/);
  });

  it("says nothing about a time it cannot read", () => {
    expect(relativeTime("not a date", now)).toBe("");
  });
});

describe("roughDuration", () => {
  it("rounds a wait to words a person would say", () => {
    expect(roughDuration(10_000)).toBe("a few seconds");
    expect(roughDuration(60_000)).toBe("a minute");
    expect(roughDuration(160_000)).toBe("3 minutes");
    expect(roughDuration(65 * 60_000)).toBe("an hour");
    expect(roughDuration(150 * 60_000)).toBe("3 hours");
  });

  it("never shows a nonsense number", () => {
    expect(roughDuration(Number.NaN)).toBe("a few seconds");
    expect(roughDuration(-5_000)).toBe("a few seconds");
  });
});
