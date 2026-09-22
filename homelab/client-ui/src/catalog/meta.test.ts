import { describe, expect, it } from "vitest";
import { APP_META, GROUPS, groupFor, groupLabel, taglineFor } from "./meta";

// This file is the storefront's whole vocabulary: 50-odd names like Vikunja,
// Karakeep and Navidrome that mean nothing to almost anyone, plus the one line
// each that turns the grid into somewhere you can find what you came for. A
// typo in a group id does not fail anything — the app just quietly lands in
// "Other", which is the one bucket nobody browses.

describe("APP_META", () => {
  const groupIds = new Set(GROUPS.map((g) => g.id));

  it("puts every app in a group that exists", () => {
    // A misspelt group silently becomes "Other" via groupLabel's fallback.
    // Nothing anywhere else notices.
    const wrong = Object.entries(APP_META)
      .filter(([, m]) => !groupIds.has(m.group))
      .map(([id, m]) => `${id} -> ${m.group}`);
    expect(wrong).toEqual([]);
  });

  it("gives every app a tagline worth reading", () => {
    const bad = Object.entries(APP_META)
      .filter(([, m]) => m.tagline.trim().length < 10)
      .map(([id]) => id);
    expect(bad).toEqual([]);
  });

  it("keeps every tagline to one line", () => {
    // These render in a fixed-height tile. A second line is clipped, so the
    // half of the sentence that says what the app is like disappears.
    const multiline = Object.entries(APP_META)
      .filter(([, m]) => m.tagline.includes("\n"))
      .map(([id]) => id);
    expect(multiline).toEqual([]);
  });

  it("leaves no group heading empty", () => {
    // An empty group renders as a heading with nothing under it, which reads
    // like the page failed to load rather than like a deliberate section.
    const used = new Set(Object.values(APP_META).map((m) => m.group));
    const empty = GROUPS.filter((g) => !used.has(g.id)).map((g) => g.id);
    expect(empty).toEqual([]);
  });

  it("has no duplicate group ids", () => {
    expect(groupIds.size).toBe(GROUPS.length);
  });
});

describe("taglineFor", () => {
  it("prefers the curated line", () => {
    const [id, meta] = Object.entries(APP_META)[0];
    expect(taglineFor({ id, description: "chart blurb" })).toBe(meta.tagline);
  });

  it("falls back to the chart's own description", () => {
    // Apps from a user-added repo have no curated line and must not be blank.
    expect(
      taglineFor({ id: "not-in-the-catalog", description: "chart blurb" }),
    ).toBe("chart blurb");
  });

  it("is empty rather than undefined when there is nothing to say", () => {
    // "undefined" rendered into a tile is worse than an empty one.
    expect(taglineFor({ id: "not-in-the-catalog" })).toBe("");
  });
});

describe("groupFor", () => {
  it("prefers the curated group", () => {
    const [id, meta] = Object.entries(APP_META)[0];
    expect(groupFor({ id, category: "media" })).toBe(meta.group);
  });

  it("maps a chart category when the app is not curated", () => {
    expect(groupFor({ id: "unknown", category: "development" })).toBe("dev");
    expect(groupFor({ id: "unknown", category: "security" })).toBe("personal");
  });

  it("lands in tools rather than nowhere", () => {
    expect(groupFor({ id: "unknown" })).toBe("tools");
    expect(groupFor({ id: "unknown", category: "nonsense" })).toBe("tools");
  });

  it("only ever returns a group that exists", () => {
    // Every fallback path included — the category map is a second place a
    // typo could put an app somewhere GROUPS has never heard of.
    const ids = new Set(GROUPS.map((g) => g.id));
    const categories = [
      "media",
      "productivity",
      "utilities",
      "monitoring",
      "communication",
      "gaming",
      "development",
      "security",
      "ai",
      "",
      "junk",
    ];
    for (const category of categories) {
      expect(ids).toContain(groupFor({ id: "unknown", category }));
    }
  });
});

describe("groupLabel", () => {
  it("names every real group", () => {
    for (const g of GROUPS) expect(groupLabel(g.id)).toBe(g.label);
  });

  it("says Other rather than nothing for a group it does not know", () => {
    expect(groupLabel("does-not-exist")).toBe("Other");
  });
});
