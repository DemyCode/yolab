import { describe, expect, it } from "vitest";
import {
  isStale,
  needsAttention,
  protectionLabel,
  protectionState,
  protectionTone,
  budgetToBytes,
  bytesToBudget,
  canDelete,
  pointPath,
  ageLabel,
  backupPagePath,
  backupServices,
  copiesLabel,
  restorePath,
  timelinePlaces,
  serviceName,
  sortRestorePoints,
  usageLabel,
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

describe("backup sizes", () => {
  it("shows the size against the budget once both are known", () => {
    expect(usageLabel(3_200_000_000, 10_000_000_000)).toBe("3.2 GB of 10 GB");
    expect(usageLabel(3_200_000_000, null)).toBe("3.2 GB");
    expect(usageLabel(null, 10_000_000_000)).toBeNull();
  });

  it("reads a budget typed in gigabytes, with a comma or a dot", () => {
    expect(budgetToBytes("10")).toBe(10_000_000_000);
    expect(budgetToBytes("2,5")).toBe(2_500_000_000);
    expect(budgetToBytes(" 0.5 ")).toBe(500_000_000);
  });

  it("refuses a budget of nothing or of words", () => {
    for (const junk of ["", "0", "-3", "lots"]) {
      expect(budgetToBytes(junk)).toBeNull();
    }
  });

  it("shows a stored budget back in gigabytes", () => {
    expect(bytesToBudget(10_000_000_000)).toBe("10");
    expect(bytesToBudget(2_500_000_000)).toBe("2.5");
  });
});

describe("restore points", () => {
  it("addresses one point under its service", () => {
    expect(pointPath("yolab-gitea-ab23", "1a2b3c4d")).toBe(
      "/api/backups/apps/yolab-gitea-ab23/points/1a2b3c4d",
    );
  });

  it("never offers to delete a point the user chose to keep", () => {
    expect(canDelete({ snapshot_id: "a", time: "", keep: true })).toBe(false);
    expect(canDelete({ snapshot_id: "a", time: "" })).toBe(true);
  });
});

describe("backupServices", () => {
  const point = { snapshot_id: "a", time: "2026-11-01T03:00:00Z" };

  it("lists running and removed services together, by name", () => {
    const got = backupServices(
      [app({ namespace: "yolab-m", instance_name: "m", app_id: "mealie" })],
      [
        {
          namespace: "yolab-z",
          instance_name: "z",
          installed: false,
          versions: [point],
        },
        {
          namespace: "yolab-m",
          instance_name: "m",
          installed: true,
          versions: [point, point],
          size_bytes: 9,
        },
        {
          namespace: "yolab-a",
          instance_name: "a",
          installed: false,
          versions: [point],
        },
      ],
    );
    expect(got.map((s) => [s.name, s.installed, s.copies])).toEqual([
      ["a", false, 1],
      ["m", true, 2],
      ["z", false, 1],
    ]);
    expect(got[1].appId).toBe("mealie");
    expect(got[1].sizeBytes).toBe(9);
  });

  it("leaves out a removed service with nothing left to restore", () => {
    const got = backupServices(
      [],
      [
        {
          namespace: "yolab-gone",
          instance_name: "gone",
          installed: false,
          versions: [],
        },
      ],
    );
    expect(got).toEqual([]);
  });

  it("shows a running service that has not been backed up yet", () => {
    const got = backupServices([app({ state: "never" })], []);
    expect(got).toHaveLength(1);
    expect(got[0].copies).toBe(0);
    expect(got[0].installed).toBe(true);
  });

  it("names a service without its namespace prefix", () => {
    expect(serviceName("yolab-gitea-ab23")).toBe("gitea-ab23");
  });

  it("counts copies in words", () => {
    expect(copiesLabel(1)).toBe("1 copy");
    expect(copiesLabel(3)).toBe("3 copies");
  });

  it("opens a service's timeline at its own address", () => {
    expect(backupPagePath("yolab-a b")).toBe("/system/backups/yolab-a%20b");
  });
});

describe("timelinePlaces", () => {
  const at = (id: string, hoursAgo: number) => ({
    snapshot_id: id,
    time: new Date(NOW - hoursAgo * 3_600_000).toISOString(),
  });

  it("puts the newest copy at the right edge and the oldest at the left", () => {
    const places = timelinePlaces([at("new", 0), at("old", 24 * 30)], NOW);
    expect(places.get("new")).toBe(1);
    expect(places.get("old")).toBe(0);
  });

  it("stretches recent time so doubling gaps look evenly spaced", () => {
    const places = timelinePlaces(
      [at("a", 0), at("b", 24), at("c", 72), at("d", 168), at("e", 360)],
      NOW,
    );
    const xs = ["a", "b", "c", "d", "e"].map((id) => places.get(id)!);
    const steps = xs.slice(1).map((x, i) => xs[i] - x);
    const later = steps.slice(1);
    expect(Math.max(...later) / Math.min(...later)).toBeLessThan(1.6);
    expect(xs.every((x) => x >= 0 && x <= 1)).toBe(true);
  });

  it("keeps a single copy, or one from the future, on the line", () => {
    const places = timelinePlaces([at("only", -2)], NOW);
    expect(places.get("only")).toBe(1);
  });
});

describe("ageLabel", () => {
  it("says how long ago in the largest unit that reads naturally", () => {
    const ago = (h: number) => new Date(NOW - h * 3_600_000).toISOString();
    expect(ageLabel(ago(0.2), NOW)).toBe("now");
    expect(ageLabel(ago(5), NOW)).toBe("5 h ago");
    expect(ageLabel(ago(72), NOW)).toBe("3 d ago");
    expect(ageLabel(ago(24 * 120), NOW)).toBe("4 mo ago");
    expect(ageLabel(ago(24 * 1000), NOW)).toBe("3 y ago");
  });
});
