import { describe, expect, it } from "vitest";

const sources = import.meta.glob<string>(
  ["../**/*.ts", "../**/*.tsx", "!../**/*.test.ts", "!../**/*.test.tsx"],
  { query: "?raw", import: "default", eager: true },
);

describe("server calls", () => {
  it("go through lib/api.ts, which handles a lost session and the server's error text", () => {
    const callers = Object.entries(sources)
      .filter(([file]) => !file.endsWith("/lib/api.ts"))
      .filter(([, text]) => /(^|[^.\w])fetch\(/m.test(text))
      .map(([file]) => file);
    expect(callers).toEqual([]);
  });

  it("are actually scanned", () => {
    expect(Object.keys(sources).some((f) => f.endsWith("/lib/api.ts"))).toBe(
      true,
    );
  });
});
