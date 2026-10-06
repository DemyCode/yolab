import { describe, expect, it } from "vitest";
import type { CatalogApp } from "@/types/apps";
import {
  COLLECTIONS,
  COLLECTION_SIZE,
  formatCount,
  githubUrl,
  hasCommunity,
  inCollection,
  shownInstalls,
  shownRating,
  sortApps,
  statsById,
  type AppStats,
} from "./store";

function app(id: string, extra: Partial<CatalogApp> = {}): CatalogApp {
  return {
    id,
    repo: "official",
    chart_version: "1.0.0",
    name: id,
    description: "",
    home: "",
    icon: "",
    category: "",
    github: "",
    tagline: "",
    collections: [],
    stars: null,
    pushed_at: null,
    schema: {},
    ...extra,
  };
}

function stats(app_id: string, extra: Partial<AppStats> = {}): AppStats {
  return {
    app_id,
    installs: 0,
    rating_count: 0,
    rating_average: null,
    comment_count: 0,
    ...extra,
  };
}

describe("formatCount", () => {
  it("shortens large counts the way people say them", () => {
    expect(formatCount(950)).toBe("950");
    expect(formatCount(1000)).toBe("1k");
    expect(formatCount(52_300)).toBe("52.3k");
    expect(formatCount(150_400)).toBe("150k");
    expect(formatCount(1_240_000)).toBe("1.2M");
  });

  it("never shows a negative or a non-number", () => {
    expect(formatCount(-3)).toBe("0");
    expect(formatCount(Number.NaN)).toBe("0");
  });
});

describe("what the store dares to show", () => {
  it("hides YoLab installs until there are enough to mean something", () => {
    expect(shownInstalls(stats("a", { installs: 9 }))).toBeNull();
    expect(shownInstalls(stats("a", { installs: 10 }))).toBe(10);
    expect(shownInstalls(undefined)).toBeNull();
  });

  it("hides a rating until three accounts have rated", () => {
    const two = stats("a", { rating_count: 2, rating_average: 5 });
    expect(shownRating(two)).toBeNull();
    const three = stats("a", { rating_count: 3, rating_average: 4.666 });
    expect(shownRating(three)).toEqual({ average: 4.7, count: 3 });
  });
});

describe("sortApps", () => {
  const apps = [
    app("b", { stars: 10, pushed_at: "2026-01-01T00:00:00Z" }),
    app("a", { stars: 500, pushed_at: "2025-01-01T00:00:00Z" }),
    app("c", { stars: null, pushed_at: "2026-09-01T00:00:00Z" }),
  ];

  it("puts the most starred first", () => {
    const order = sortApps(apps, "popular", new Map());
    expect(order.map((a) => a.id)).toEqual(["a", "b", "c"]);
  });

  it("lets enough YoLab installs outrank stars", () => {
    const byId = statsById([stats("c", { installs: 12 })]);
    expect(sortApps(apps, "popular", byId)[0].id).toBe("c");
  });

  it("orders by most recent update", () => {
    const order = sortApps(apps, "updated", new Map());
    expect(order.map((a) => a.id)).toEqual(["c", "b", "a"]);
  });

  it("orders by name", () => {
    const order = sortApps(apps, "name", new Map());
    expect(order.map((a) => a.id)).toEqual(["a", "b", "c"]);
  });

  it("never reorders the list it was given", () => {
    sortApps(apps, "name", new Map());
    expect(apps.map((a) => a.id)).toEqual(["b", "a", "c"]);
  });
});

describe("inCollection", () => {
  it("keeps the collection's apps, most popular first, one row", () => {
    const many = Array.from({ length: COLLECTION_SIZE + 2 }, (_, i) =>
      app(`x${i}`, { collections: ["play"], stars: i }),
    );
    const row = inCollection([...many, app("other")], "play", new Map());
    expect(row).toHaveLength(COLLECTION_SIZE);
    expect(row[0].id).toBe(`x${COLLECTION_SIZE + 1}`);
  });

  it("names every collection the charts are allowed to use", () => {
    expect(COLLECTIONS.map((c) => c.id)).toEqual([
      "start-here",
      "replace-google",
      "family",
      "watch-and-listen",
      "privacy",
      "for-developers",
      "play",
    ]);
  });
});

describe("links and community", () => {
  it("links only a real owner/repo path to GitHub", () => {
    expect(githubUrl({ github: "immich-app/immich" })).toBe(
      "https://github.com/immich-app/immich",
    );
    expect(githubUrl({ github: "" })).toBeNull();
    expect(githubUrl({ github: "javascript:alert(1)" })).toBeNull();
  });

  it("opens ratings and comments only for official catalog apps", () => {
    expect(hasCommunity({ repo: "official" })).toBe(true);
    expect(hasCommunity({ repo: "custom" })).toBe(false);
  });
});
