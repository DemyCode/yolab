import { describe, expect, it } from "vitest";
import {
  APP_GROUP,
  GROUPS,
  countByGroup,
  groupFor,
  groupLabel,
  taglineFor,
} from "./meta";

describe("APP_GROUP", () => {
  const groupIds = new Set(GROUPS.map((g) => g.id));

  it("puts every app in a group that exists", () => {
    const wrong = Object.entries(APP_GROUP)
      .filter(([, group]) => !groupIds.has(group))
      .map(([id, group]) => `${id} -> ${group}`);
    expect(wrong).toEqual([]);
  });

  it("leaves no group heading empty", () => {
    const used = new Set(Object.values(APP_GROUP));
    const empty = GROUPS.filter((g) => !used.has(g.id)).map((g) => g.id);
    expect(empty).toEqual([]);
  });

  it("has no duplicate group ids", () => {
    expect(groupIds.size).toBe(GROUPS.length);
  });
});

describe("taglineFor", () => {
  it("uses the line the chart itself declares", () => {
    expect(
      taglineFor({ tagline: "Your own Google Photos", description: "blurb" }),
    ).toBe("Your own Google Photos");
  });

  it("falls back to the chart's description when it declares no line", () => {
    expect(taglineFor({ description: "chart blurb" })).toBe("chart blurb");
    expect(taglineFor({ tagline: "  ", description: "chart blurb" })).toBe(
      "chart blurb",
    );
  });

  it("is empty rather than undefined when there is nothing to say", () => {
    expect(taglineFor({})).toBe("");
  });
});

describe("groupFor", () => {
  it("prefers the curated group", () => {
    const [id, group] = Object.entries(APP_GROUP)[0];
    expect(groupFor({ id, category: "media" })).toBe(group);
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

describe("countByGroup", () => {
  it("counts each app once, under the group its chip filters to", () => {
    const counts = countByGroup([
      { id: "immich" },
      { id: "photoprism" },
      { id: "jellyfin" },
      { id: "unknown-game", category: "gaming" },
      { id: "unknown-thing" },
    ]);
    expect(Object.fromEntries(counts)).toEqual({
      photos: 2,
      watch: 1,
      home: 1,
      tools: 1,
    });
  });
});
