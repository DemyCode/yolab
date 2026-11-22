import { describe, expect, it } from "vitest";

const sources = import.meta.glob<string>(
  [
    "/src/**/*.ts",
    "/src/**/*.tsx",
    "!/src/**/*.test.ts",
    "!/src/**/*.test.tsx",
  ],
  { query: "?raw", import: "default", eager: true },
);

describe("server calls", () => {
  it("go through lib/api.ts, which handles a lost session and the server's error text", () => {
    const callers = Object.entries(sources)
      .filter(([file]) => file !== "/src/lib/api.ts")
      .filter(([, text]) => /(^|[^.\w])fetch\(/m.test(text))
      .map(([file]) => file);
    expect(callers).toEqual([]);
  });

  it("are actually scanned", () => {
    expect(Object.keys(sources)).toContain("/src/lib/api.ts");
  });
});
