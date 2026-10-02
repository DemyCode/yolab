import { describe, expect, it } from "vitest";
import {
  diskState,
  estimateChange,
  fillTone,
  formatCephBytes,
  pickBanner,
  placesFor,
  protectionLine,
  rawPercent,
  statusLine,
  usageRows,
} from "./storage";
import type { AppInfo, CatalogApp } from "@/types/apps";
import type {
  DiskInfo,
  Osd,
  Space,
  StorageOverview,
  StorageTarget,
} from "@/types/storage";

const GiB = 1024 ** 3;

function osd(over: Partial<Osd> = {}): Osd {
  return {
    id: 0,
    name: "osd.0",
    host: "node1",
    class: "ssd",
    size_bytes: 100 * GiB,
    used_bytes: 10 * GiB,
    avail_bytes: 90 * GiB,
    utilization: 10,
    var: 1,
    pgs: 32,
    up: true,
    weight: 1,
    reweight: 1,
    ...over,
  };
}

function space(over: Partial<Space> = {}): Space {
  return {
    free_bytes: 100 * GiB,
    apps_bytes: 40 * GiB,
    images_bytes: 10 * GiB,
    other_bytes: 0,
    copies: 2,
    fullest_disk_percent: 30,
    ...over,
  };
}

function overview(over: Partial<StorageOverview> = {}): StorageOverview {
  return {
    health: "HEALTH_OK",
    space: space(),
    raw: {
      total_bytes: 400 * GiB,
      used_bytes: 100 * GiB,
      avail_bytes: 300 * GiB,
      data_bytes: 50 * GiB,
    },
    osds: [osd({ id: 0 }), osd({ id: 1, host: "node2" })],
    pools: [],
    ...over,
  };
}

function target(over: Partial<StorageTarget> = {}): StorageTarget {
  return {
    size: 2,
    min_size: 1,
    failure_domain: "host",
    mon: 1,
    mgr: 1,
    ...over,
  };
}

function disk(over: Partial<DiskInfo> = {}): DiskInfo {
  return {
    id: "d1",
    device: "/dev/sda",
    model: "Disk",
    size_bytes: 1e12,
    is_loop: false,
    is_our_osd: false,
    foreign_ceph: false,
    ownership: "blank",
    osd_id: null,
    desired: "OFF",
    connected: true,
    has_partitions: false,
    mounted: false,
    phase: "",
    message: "",
    attempts: 0,
    ...over,
  };
}

function app(over: Partial<AppInfo> = {}): AppInfo {
  return {
    app_id: "immich",
    instance_name: "immich",
    chart_version: "1.0.0",
    status: "running",
    detail: "",
    outputs: [],
    config: {},
    backup: { enabled: false, schedule: "", last_ok_at: null, running: false },
    ...over,
  };
}

const catalog: CatalogApp[] = [
  {
    id: "immich",
    repo: "yolab",
    chart_version: "1.0.0",
    name: "Immich",
    description: "",
    home: "",
    icon: "immich.svg",
    category: "media",
    schema: {},
  },
];

describe("formatCephBytes", () => {
  it("uses binary units so figures read the same as `ceph status`", () => {
    expect(formatCephBytes(1024)).toBe("1 KiB");
    expect(formatCephBytes(404 * GiB)).toBe("404 GiB");
    expect(formatCephBytes(1.5 * 1024 * GiB)).toBe("1.5 TiB");
  });

  it("never renders a non-number as a size", () => {
    expect(formatCephBytes(Number.NaN)).toBe("0 B");
    expect(formatCephBytes(-5)).toBe("0 B");
  });
});

describe("rawPercent", () => {
  it("is ceph's used over ceph's total", () => {
    expect(rawPercent(overview().raw)).toBe(25);
  });

  it("is zero, not NaN, before there is any capacity", () => {
    expect(
      rawPercent({
        total_bytes: 0,
        used_bytes: 0,
        avail_bytes: 0,
        data_bytes: 0,
      }),
    ).toBe(0);
  });
});

describe("fillTone", () => {
  it("warns at 70% and turns bad at 85%", () => {
    expect([69, 70, 84, 85].map(fillTone)).toEqual([
      "ok",
      "warn",
      "warn",
      "bad",
    ]);
  });
});

describe("placesFor", () => {
  const osds = [
    osd({ id: 0, host: "a" }),
    osd({ id: 1, host: "a" }),
    osd({ id: 2, host: "b" }),
    osd({ id: 3, host: "c", weight: 0 }),
  ];

  it("counts only disks that are holding data", () => {
    expect(placesFor(osds, "osd")).toBe(3);
  });

  it("counts machines with at least one such disk", () => {
    expect(placesFor(osds, "host")).toBe(2);
  });
});

describe("statusLine", () => {
  it("summarises a healthy cluster from ceph's own figures", () => {
    expect(statusLine(overview(), target())).toEqual({
      tone: "ok",
      text: "Healthy · 2 copies of everything · 2 disks · 2 machines",
    });
  });

  it("leaves the machine count out on a single machine", () => {
    const o = overview({ osds: [osd()] });
    expect(statusLine(o, target())?.text).toBe(
      "Healthy · 2 copies of everything · 1 disk",
    );
  });

  it("maps ceph's warning and error health to their tones", () => {
    const warn = statusLine(overview({ health: "HEALTH_WARN" }), null);
    const err = statusLine(overview({ health: "HEALTH_ERR" }), null);
    expect([warn?.tone, err?.tone]).toEqual(["warn", "bad"]);
  });

  it("says nothing before the first answer", () => {
    expect(statusLine(undefined, target())).toBeNull();
  });
});

describe("pickBanner", () => {
  const base = {
    error: null,
    movementBlocked: false,
    overview: overview(),
    copies: 2,
  };

  it("shows nothing when nothing needs attention", () => {
    expect(pickBanner(base)).toBeNull();
  });

  it("puts an unanswering cluster above everything else", () => {
    const b = pickBanner({
      ...base,
      error: "timed out.",
      movementBlocked: true,
    });
    expect(b).toMatchObject({
      tone: "error",
      title: "Storage is not answering",
    });
  });

  it("puts blocked data movement above an offline disk", () => {
    const o = overview({ osds: [osd(), osd({ id: 1, up: false })] });
    expect(pickBanner({ ...base, overview: o, movementBlocked: true })).toBe(
      "movement",
    );
  });

  it("an offline disk with a second copy is a warning", () => {
    const o = overview({ osds: [osd(), osd({ id: 1, up: false })] });
    expect(pickBanner({ ...base, overview: o })).toMatchObject({
      tone: "warning",
      title: "A disk is offline",
    });
  });

  it("an offline disk with only one copy is an error", () => {
    const o = overview({ osds: [osd(), osd({ id: 1, up: false })] });
    expect(pickBanner({ ...base, overview: o, copies: 1 })).toMatchObject({
      tone: "error",
    });
  });

  it("a disk ceph never placed on a machine reads as unfinished setup", () => {
    const o = overview({ osds: [osd(), osd({ id: 1, up: false, host: "" })] });
    expect(pickBanner({ ...base, overview: o })).toMatchObject({
      title: "A disk did not finish being set up",
    });
  });
});

describe("protectionLine", () => {
  it("one copy is called out as unprotected", () => {
    expect(protectionLine(target({ size: 1 }), 0)?.tone).toBe("bad");
  });

  it("names the unit copies are spread across", () => {
    expect(protectionLine(target({ size: 2 }), 0)?.text).toContain(
      "Any one machine can fail",
    );
    expect(
      protectionLine(target({ size: 3, failure_domain: "osd" }), 0)?.text,
    ).toContain("Any 2 disks can fail");
  });

  it("warns while a disk is offline", () => {
    expect(protectionLine(target(), 1)).toMatchObject({ tone: "warn" });
  });
});

describe("estimateChange", () => {
  it("adding a copy costs one more copy of everything stored", () => {
    const e = estimateChange(space(), 3, 3);
    expect(e.extraNeeded).toBe(50 * GiB);
    expect(e.freeAfter).toBeCloseTo(((200 - 50) * GiB) / 3);
    expect(e.fit).toBe("ok");
  });

  it("removing a copy frees room and needs nothing", () => {
    const e = estimateChange(space(), 1, 3);
    expect(e.extraNeeded).toBe(0);
    expect(e.freeAfter).toBe(200 * GiB);
  });

  it("copies beyond the places available are not counted", () => {
    expect(estimateChange(space(), 5, 2)).toEqual(
      estimateChange(space(), 2, 2),
    );
  });

  it("refuses when there is not enough raw room", () => {
    const e = estimateChange(space({ free_bytes: 20 * GiB }), 3, 3);
    expect(e.fit).toBe("impossible");
    expect(e.freeAfter).toBe(0);
  });

  it("flags a tight fit inside 30% headroom", () => {
    expect(estimateChange(space({ free_bytes: 30 * GiB }), 3, 3).fit).toBe(
      "tight",
    );
  });
});

describe("usageRows", () => {
  it("names an app's volumes after the app and links its instance", () => {
    const rows = usageRows(
      {
        apps: [{ namespace: "yolab-immich", instance: "immich", bytes: 5 }],
        unreadable: 0,
      },
      [app()],
      catalog,
      0,
    );
    expect(rows).toEqual([
      {
        key: "yolab-immich",
        label: "Immich",
        bytes: 5,
        instance: "immich",
        appId: "immich",
        icon: "immich.svg",
      },
    ]);
  });

  it("gathers volumes of no installed app into one row", () => {
    const rows = usageRows(
      {
        apps: [
          { namespace: "yolab-gone", instance: "gone", bytes: 3 },
          { namespace: "kube-system", instance: null, bytes: 4 },
        ],
        unreadable: 0,
      },
      [app()],
      catalog,
      0,
    );
    expect(rows).toEqual([
      { key: "other", label: "Removed or system volumes", bytes: 7 },
    ]);
  });

  it("lists container images as their own row and sorts largest first", () => {
    const rows = usageRows(
      {
        apps: [{ namespace: "yolab-immich", instance: "immich", bytes: 5 }],
        unreadable: 0,
      },
      [app()],
      catalog,
      9,
    );
    expect(rows.map((r) => r.key)).toEqual(["images", "yolab-immich"]);
  });
});

describe("diskState", () => {
  it("a connected disk switched on with its OSD running is in use", () => {
    expect(
      diskState(disk({ desired: "ON", is_our_osd: true, phase: "active" })),
    ).toBe("active");
  });

  it("a disk switched on but not connected is missing", () => {
    expect(diskState(disk({ desired: "ON", connected: false }))).toBe(
      "missing",
    );
  });

  it("a disk the machine cannot identify says so before anything else", () => {
    expect(diskState(disk({ ownership: "unknown", foreign_ceph: true }))).toBe(
      "unidentified",
    );
  });
});
