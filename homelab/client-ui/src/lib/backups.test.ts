import { describe, expect, it } from "vitest";
import {
  isBusy,
  isStale,
  needsAttention,
  protectionLabel,
  protectionState,
  protectionTone,
  restorePath,
  sortRestorePoints,
} from "./backups";
import type { ProtectedApp } from "./backups";

function app(over: Partial<ProtectedApp> = {}): ProtectedApp {
  return {
    namespace: "yolab-gitea-ab23",
    instance_name: "gitea-ab23",
    app_id: "gitea",
    enabled: true,
    schedule: "0 3 * * *",
    state: "ok",
    last_ok_at: "2026-09-24T03:00:00Z",
    error: null,
    ...over,
  };
}

const NOW = new Date("2026-09-24T12:00:00Z").getTime();

describe("protectionState", () => {
  it("reports an app with backups turned off as off, whatever its history", () => {
    expect(protectionState(app({ enabled: false, state: "ok" }))).toBe("off");
    expect(protectionState(app({ enabled: false, state: "failed" }))).toBe(
      "off",
    );
  });

  it("passes through the server's state when backups are on", () => {
    for (const state of [
      "ok",
      "running",
      "queued",
      "failed",
      "never",
    ] as const) {
      expect(protectionState(app({ state }))).toBe(state);
    }
  });
});

describe("protectionLabel", () => {
  it("has a plain-language label for every state", () => {
    for (const state of [
      "ok",
      "running",
      "queued",
      "failed",
      "never",
      "off",
    ] as const) {
      expect(protectionLabel(state).length).toBeGreaterThan(0);
    }
  });

  it("says a queued app is waiting, not that it failed", () => {
    expect(protectionLabel("queued")).toMatch(/wait/i);
  });
});

describe("protectionTone", () => {
  it("only a real failure reads as danger", () => {
    expect(protectionTone("failed")).toBe("danger");
    expect(protectionTone("ok")).toBe("success");
    expect(protectionTone("queued")).toBe("info");
    expect(protectionTone("running")).toBe("info");
    expect(protectionTone("off")).toBe("muted");
    expect(protectionTone("never")).toBe("muted");
  });
});

describe("isBusy", () => {
  it("covers both the running and the waiting case", () => {
    expect(isBusy("running")).toBe(true);
    expect(isBusy("queued")).toBe(true);
    expect(isBusy("ok")).toBe(false);
  });
});

describe("isStale", () => {
  it("a recent success is not stale", () => {
    expect(isStale(app(), NOW, 24)).toBe(false);
  });

  it("a success older than the window is stale", () => {
    expect(isStale(app({ last_ok_at: "2026-09-22T03:00:00Z" }), NOW, 24)).toBe(
      true,
    );
  });

  it("a failure is stale even if an older success exists", () => {
    expect(isStale(app({ state: "failed" }), NOW, 24)).toBe(true);
  });

  it("an app that has never been saved is stale", () => {
    expect(isStale(app({ state: "never", last_ok_at: null }), NOW, 24)).toBe(
      true,
    );
  });

  it("an app the user turned backups off for is never nagged about", () => {
    expect(
      isStale(
        app({ enabled: false, state: "never", last_ok_at: null }),
        NOW,
        24,
      ),
    ).toBe(false);
    expect(isStale(app({ enabled: false, state: "failed" }), NOW, 24)).toBe(
      false,
    );
  });

  it("an app mid-save is not stale just because it has no success yet", () => {
    expect(isStale(app({ state: "running", last_ok_at: null }), NOW, 24)).toBe(
      false,
    );
  });
});

describe("needsAttention", () => {
  it("names only the apps a person has to do something about", () => {
    const apps = [
      app({ instance_name: "fine" }),
      app({ instance_name: "broken", state: "failed" }),
      app({ instance_name: "ignored", enabled: false, state: "failed" }),
    ];
    expect(needsAttention(apps, NOW, 24).map((a) => a.instance_name)).toEqual([
      "broken",
    ]);
  });

  it("is empty when everything is saved", () => {
    expect(needsAttention([app(), app()], NOW, 24)).toEqual([]);
  });
});

describe("restorePath", () => {
  it("points at the install screen for that app and backup", () => {
    expect(restorePath("gitea", "yolab-gitea-ab23", "deadbeef")).toBe(
      "/add/gitea?restore=yolab-gitea-ab23&snapshot=deadbeef",
    );
  });

  it("escapes anything that would break the query string", () => {
    const path = restorePath("gitea", "yolab-a&b", "x y");
    expect(path).toContain("restore=yolab-a%26b");
    expect(path).toContain("snapshot=x%20y");
  });
});

describe("sortRestorePoints", () => {
  it("puts the newest point first whatever order the server sent", () => {
    const sorted = sortRestorePoints([
      { snapshot_id: "old", time: "2026-09-20T03:00:00Z" },
      { snapshot_id: "new", time: "2026-09-24T03:00:00Z" },
      { snapshot_id: "mid", time: "2026-09-22T03:00:00Z" },
    ]);
    expect(sorted.map((p) => p.snapshot_id)).toEqual(["new", "mid", "old"]);
  });

  it("does not mutate what it was given", () => {
    const points = [
      { snapshot_id: "old", time: "2026-09-20T03:00:00Z" },
      { snapshot_id: "new", time: "2026-09-24T03:00:00Z" },
    ];
    sortRestorePoints(points);
    expect(points[0].snapshot_id).toBe("old");
  });
});
