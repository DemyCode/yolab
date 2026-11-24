import { describe, expect, it } from "vitest";
import {
  isStale,
  needsAttention,
  protectionLabel,
  protectionState,
  protectionTone,
  budgetToBytes,
  bytesToBudget,
  pointPath,
  ageLabel,
  backupPagePath,
  backupServices,
  copiesLabel,
  restorePath,
  byStorage,
  scheduleLabel,
  serviceBanner,
  serviceDetail,
  serviceTone,
  spaceToFit,
  storageTotal,
  serviceName,
  sortRestorePoints,
  usageLabel,
} from "./backups";
import type { BackupService, ProtectedApp, RestorePoints } from "./backups";

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

describe("the services list", () => {
  const service = (
    protection: ProtectedApp | null,
    over: Partial<BackupService> = {},
  ): BackupService => ({
    namespace: "yolab-gitea-ab23",
    name: "gitea-ab23",
    appId: protection ? "gitea" : null,
    installed: protection !== null,
    protection,
    copies: 4,
    sizeBytes: null,
    ...over,
  });

  it("says when a saved service was last saved and how often it is", () => {
    const s = service(app({ last_ok_at: "2026-09-24T09:00:00Z" }));
    expect(serviceDetail(s, NOW)).toBe("Saved 3 h ago · Every day at 03:00");
    expect(serviceTone(s, NOW)).toBe("ok");
  });

  it("says how many copies a removed service still leaves behind", () => {
    expect(serviceDetail(service(null), NOW)).toBe("Removed · 4 copies left");
    expect(serviceTone(service(null), NOW)).toBe("ok");
  });

  it("says a service with backups off still keeps its old copies", () => {
    const off = service(app({ enabled: false }));
    expect(serviceDetail(off, NOW)).toBe("Backups off · 4 copies kept");
    expect(
      serviceDetail(service(app({ enabled: false }), { copies: 0 }), NOW),
    ).toBe("Backups off");
  });

  it("flags a service whose copies need more than its space", () => {
    const over = service(app({ over_budget: true }));
    expect(serviceDetail(over, NOW)).toContain("More than its space");
    expect(serviceTone(over, NOW)).toBe("warn");
  });

  it("shows a failed save in red, an old one and a running one apart", () => {
    expect(serviceTone(service(app({ state: "failed" })), NOW)).toBe("error");
    expect(serviceDetail(service(app({ state: "failed" })), NOW)).toBe(
      "Last save failed",
    );
    const old = app({ last_ok_at: "2026-09-20T03:00:00Z" });
    expect(serviceTone(service(old), NOW)).toBe("warn");
    expect(serviceTone(service(app({ state: "running" })), NOW)).toBe("busy");
  });

  it("puts what takes the most backup storage first, unmeasured last", () => {
    const got = byStorage([
      service(app(), { name: "small", sizeBytes: 5 }),
      service(app(), { name: "unknown", sizeBytes: null }),
      service(app(), { name: "big", sizeBytes: 167e9 }),
      service(app(), { name: "also-small", sizeBytes: 5 }),
    ]);
    expect(got.map((s) => s.name)).toEqual([
      "big",
      "also-small",
      "small",
      "unknown",
    ]);
  });

  it("adds up the storage every service's backups take", () => {
    expect(
      storageTotal([
        service(app(), { sizeBytes: 2 }),
        service(null, { sizeBytes: 3 }),
        service(app(), { sizeBytes: null }),
      ]),
    ).toBe(5);
    expect(storageTotal([service(app())])).toBeNull();
  });

  it("names a preset schedule and shows any other one as it was written", () => {
    expect(scheduleLabel("0 3 1 * *")).toBe("On the 1st of each month");
    expect(scheduleLabel("15 4 * * 2")).toBe("Custom: 15 4 * * 2");
  });
});

describe("serviceBanner", () => {
  const copies = (over: Partial<RestorePoints> = {}): RestorePoints => ({
    configured: true,
    points: [],
    ...over,
  });

  it("shows nothing for a removed service or a healthy one", () => {
    expect(serviceBanner(undefined, copies(), NOW)).toBeNull();
    expect(serviceBanner(app(), copies(), NOW)).toBeNull();
  });

  it("puts a failed backup above everything else", () => {
    const got = serviceBanner(
      app({ state: "failed", error: "disk full" }),
      copies({ over_budget: true, size_bytes: 9, budget_bytes: 1 }),
      NOW,
    );
    expect(got).toEqual({ kind: "failed", error: "disk full" });
  });

  it("then copies that need more than their space, with both sizes", () => {
    expect(
      serviceBanner(
        app({ last_ok_at: "2026-09-01T03:00:00Z" }),
        copies({ over_budget: true, size_bytes: 9, budget_bytes: 1 }),
        NOW,
      ),
    ).toEqual({ kind: "over", sizeBytes: 9, budgetBytes: 1 });
  });

  it("then a service with no recent copy", () => {
    expect(
      serviceBanner(app({ last_ok_at: "2026-09-01T03:00:00Z" }), copies(), NOW),
    ).toEqual({ kind: "stale" });
  });

  it("never nags about a service whose backups were turned off", () => {
    expect(
      serviceBanner(
        app({ enabled: false, state: "failed", last_ok_at: null }),
        copies(),
        NOW,
      ),
    ).toBeNull();
  });
});

describe("spaceToFit", () => {
  it("rounds up to the next whole gigabyte so no copy has to go", () => {
    expect(spaceToFit(167_182_292_770)).toBe(168_000_000_000);
    expect(spaceToFit(10)).toBe(1_000_000_000);
  });
});

describe("backups edge cases", () => {
  it("a stored service without a name is named after its namespace", () => {
    const [got] = backupServices(
      [],
      [
        {
          namespace: "yolab-gone",
          instance_name: "",
          installed: true,
          versions: [],
        },
      ],
    );
    expect(got.name).toBe(serviceName("yolab-gone"));
  });

  it("a service never saved says only that it is saved", () => {
    const service: BackupService = {
      namespace: "ns",
      name: "n",
      appId: "gitea",
      installed: true,
      protection: app({ last_ok_at: null }),
      copies: 1,
      sizeBytes: null,
    };
    expect(serviceDetail(service, NOW)).toBe(
      `Saved · ${scheduleLabel("0 3 * * *")}`,
    );
  });

  it("a failed backup without a reason still says it failed", () => {
    expect(
      serviceBanner(app({ state: "failed", error: null }), null, NOW),
    ).toEqual({ kind: "failed", error: "The last backup failed." });
  });
});
