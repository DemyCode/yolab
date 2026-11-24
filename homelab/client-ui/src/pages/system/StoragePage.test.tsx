import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { StoragePage } from "./StoragePage";
import { fakeServer, never, reply } from "@/test/server";
import { renderAt } from "@/test/render";
import { appInfo, catalogApp } from "@/test/fixtures";
import type {
  DiskInfo,
  Movement,
  Osd,
  StorageOverview,
  StoragePolicyData,
  VdiskInfo,
} from "@/types/storage";

vi.mock("@/components/ForceHeal", () => ({ ForceHealCard: () => null }));
vi.mock("@/components/DataMovement", () => ({
  DataMovementCard: ({ className }: { className?: string }) => (
    <p data-testid="movement">{className ? "below" : "above"}</p>
  ),
  DrainProgress: () => <span> · half moved</span>,
}));

const osd = (over: Partial<Osd> = {}): Osd => ({
  id: 0,
  name: "osd.0",
  host: "node1",
  class: "ssd",
  size_bytes: 1e12,
  used_bytes: 2e11,
  avail_bytes: 8e11,
  utilization: 20,
  var: 1,
  pgs: 32,
  up: true,
  weight: 1,
  reweight: 1,
  ...over,
});

const overview = (over: Partial<StorageOverview> = {}): StorageOverview => ({
  health: "HEALTH_OK",
  space: {
    free_bytes: 5e11,
    apps_bytes: 1e11,
    images_bytes: 5e10,
    other_bytes: 0,
    copies: 2,
    fullest_disk_percent: 20,
  },
  raw: {
    total_bytes: 2e12,
    used_bytes: 4e11,
    avail_bytes: 1.6e12,
    data_bytes: 2e11,
  },
  osds: [osd(), osd({ id: 1, name: "osd.1" })],
  pools: [],
  ...over,
});

const disk = (over: Partial<DiskInfo> = {}): DiskInfo => ({
  id: "d1",
  device: "/dev/sda",
  model: "Samsung",
  size_bytes: 1e12,
  is_loop: false,
  is_our_osd: true,
  foreign_ceph: false,
  osd_id: 0,
  desired: "ON",
  connected: true,
  has_partitions: false,
  mounted: false,
  phase: "active",
  message: "",
  attempts: 0,
  ...over,
});

const vdisk = (over: Partial<VdiskInfo> = {}): VdiskInfo => ({
  id: 1,
  disk_id: "v1",
  node: "node1",
  box_type: "bx21",
  size_gb: 5000,
  ready: true,
  state: "connected",
  status: "",
  ...over,
});

const policy = (over: Partial<StoragePolicyData> = {}): StoragePolicyData => ({
  policy: { size: 2, failure_domain: "osd" },
  topology: null,
  target: { size: 2, min_size: 1, failure_domain: "osd", mon: 1, mgr: 1 },
  ...over,
});

const movement = (over: Partial<Movement> = {}): Movement => ({
  state: "settled",
  jobs: [],
  move_reason: null,
  draining: [],
  filling: [],
  waiting_for: [],
  copies: null,
  clones: 0,
  repairing: false,
  eta_secs: null,
  inactive_pgs: 0,
  total_pgs: 0,
  ...over,
});

function serve(over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/storage/movement": movement(),
    "GET /api/storage": overview(),
    "GET /api/storage/policy": policy(),
    "GET /api/disks": { node1: [disk()] },
    "GET /api/vdisks": [],
    "GET /api/storage/usage": { apps: [], unreadable: 0 },
    "GET /api/apps": [],
    "GET /api/apps/catalog": [],
    "GET /api/storage/ceph-status": { text: "HEALTH_OK all good" },
    "GET /api/storage/checks": { ok_to_stop: [0], safe_to_destroy: [] },
    "GET /api/ceph/dashboard": { username: "admin", password: "s3cret" },
    "GET /api/vdisks/offers": [
      { box_type: "bx11", size_gb: 1000, monthly_cents: 390 },
    ],
    ...over,
  });
}

const open = () => renderAt(<StoragePage />, { path: "/system/storage" });
const section = (title: string) =>
  screen.getByRole("heading", { name: title }).closest("section")!;

describe("StoragePage, the summary", () => {
  it("shows placeholders while everything loads", () => {
    fakeServer({});
    const all = never();
    serve({
      "GET /api/storage": all,
      "GET /api/storage/policy": all,
      "GET /api/disks": all,
      "GET /api/vdisks": all,
      "GET /api/storage/usage": all,
      "GET /api/storage/movement": all,
    });
    const { container } = open();
    expect(container.querySelectorAll(".shimmer").length).toBeGreaterThan(5);
    expect(screen.queryByText("Add virtual disk")).toBeNull();
    expect(screen.queryByText("Protection")).toBeNull();
  });

  it("says how healthy storage is, how much is free and how it is stored", async () => {
    serve({
      "GET /api/storage": overview({
        osds: [osd(), osd({ id: 1, host: "node2" })],
        space: {
          ...overview().space!,
          other_bytes: 1e9,
          fullest_disk_percent: 72.4,
        },
      }),
    });
    open();
    expect(
      await screen.findByText(
        "Healthy · 2 copies of everything · 2 disks · 2 machines",
      ),
    ).toBeTruthy();
    expect(screen.getByText("free for your files")).toBeTruthy();
    expect(screen.getByText(/· Other/)).toBeTruthy();
    expect(
      screen.getByText(/Everything is stored 2 times, so the disks hold/),
    ).toBeTruthy();
    const fullest = screen.getByText(/Your fullest disk is/);
    expect(fullest.textContent).toContain("72% full");
    expect(fullest.className).toContain("text-warning");
    expect(screen.getByTestId("movement").textContent).toBe("below");
  });

  it("says when everything is stored once, with nothing else besides", async () => {
    serve({
      "GET /api/storage": overview({
        health: "HEALTH_ERR",
        osds: [osd()],
        space: { ...overview().space!, copies: 1 },
      }),
    });
    open();
    expect(
      await screen.findByText("Needs you now · 1 copy of everything · 1 disk"),
    ).toBeTruthy();
    expect(screen.getByText(/Everything is stored once, so/)).toBeTruthy();
    expect(screen.queryByText(/· Other/)).toBeNull();
    expect(screen.queryByText(/Your fullest disk/)).toBeNull();
  });

  it("says when storage is still being set up", async () => {
    serve({
      "GET /api/storage": overview({ health: "HEALTH_WARN", space: null }),
    });
    open();
    expect(
      await screen.findByText("Storage is still being set up"),
    ).toBeTruthy();
    expect(
      screen.getByText("Needs attention · 2 copies of everything · 2 disks"),
    ).toBeTruthy();
  });

  it("names what is moving in the summary", async () => {
    serve({
      "GET /api/storage": overview({ health: "HEALTH_WARN" }),
      "GET /api/storage/movement": movement({
        state: "working",
        jobs: [
          {
            kind: "move",
            unit: "bytes",
            remaining_bytes: 5,
            to_move_bytes: 10,
            moved_bytes: 5,
            progress: 0.5,
            eta_secs: null,
          },
        ],
      }),
    });
    open();
    await waitFor(() =>
      expect(document.querySelector("p.-mt-4")!.textContent).not.toMatch(
        /^Needs attention/,
      ),
    );
    expect(document.querySelector("p.-mt-4")!.textContent).toMatch(
      /· 2 copies of everything · 2 disks$/,
    );
  });

  it("puts the data movement card on top when it needs you", async () => {
    serve({ "GET /api/storage/movement": movement({ state: "no_room" }) });
    open();
    await waitFor(() =>
      expect(screen.getByTestId("movement").textContent).toBe("above"),
    );
    expect(screen.getAllByTestId("movement")).toHaveLength(1);
  });

  it("says when storage is not answering", async () => {
    serve({ "GET /api/storage": reply(502, '{"error":"mon down"}') });
    open();
    expect(await screen.findByText("Storage is not answering")).toBeTruthy();
    expect(screen.getByText(/\(mon down\)/)).toBeTruthy();
  });

  it("warns about an offline disk with no second copy, falling back to the policy's copies", async () => {
    serve({
      "GET /api/storage": overview({ space: null, osds: [osd({ up: false })] }),
      "GET /api/storage/policy": policy({ policy: null, target: null }),
    });
    open();
    expect(
      await screen.findByText("A disk is offline and there is no second copy"),
    ).toBeTruthy();
  });
});

describe("StoragePage, what uses space", () => {
  it("lists services by size, with programs and the rest apart", async () => {
    serve({
      "GET /api/storage/usage": {
        apps: [
          { namespace: "yolab-zz", instance: "zz", bytes: 4e10 },
          { namespace: "yolab-x", instance: null, bytes: 1e9 },
        ],
        unreadable: 1,
      },
      "GET /api/apps": [appInfo({ app_id: "zz", instance_name: "zz" })],
      "GET /api/apps/catalog": [
        catalogApp({ id: "zz", name: "Zed", icon: "🍵" }),
      ],
    });
    open();
    const usage = await screen.findByText("Zed");
    expect(screen.getByText("🍵")).toBeTruthy();
    expect(screen.getByText("Service programs")).toBeTruthy();
    expect(
      screen.getByText("1 volume could not be measured this time."),
    ).toBeTruthy();
    fireEvent.click(usage);
    expect(screen.getByTestId("where").textContent).toBe("/app/zz");
  });

  it("says when nothing is stored, and how many volumes could not be measured", async () => {
    serve({
      "GET /api/storage": overview({ space: null }),
      "GET /api/storage/usage": { apps: [], unreadable: 2 },
    });
    open();
    expect(
      await screen.findByText("No service is storing anything yet"),
    ).toBeTruthy();
    expect(
      screen.getByText("2 volumes could not be measured this time."),
    ).toBeTruthy();
  });

  it("draws no bar for a service that stores nothing", async () => {
    serve({
      "GET /api/storage": overview({ space: null }),
      "GET /api/storage/usage": {
        apps: [{ namespace: "yolab-gitea", instance: "gitea", bytes: 0 }],
        unreadable: 0,
      },
      "GET /api/apps": [appInfo({ app_id: "gitea", instance_name: "gitea" })],
      "GET /api/apps/catalog": reply(500, ""),
    });
    open();
    const row = (await screen.findByText("gitea")).closest("a")!;
    expect((row.querySelector("[style]") as HTMLElement).style.width).toBe(
      "0%",
    );
  });

  it("says why the sizes could not be read", async () => {
    serve({ "GET /api/storage/usage": reply(500, '{"error":"du failed"}') });
    open();
    expect(
      await screen.findByText("Service sizes could not be read right now"),
    ).toBeTruthy();
    expect(screen.getByText("du failed")).toBeTruthy();
  });
});

describe("StoragePage, disks", () => {
  it("says when there are no disks, and offers no virtual disk without a machine", async () => {
    serve({ "GET /api/disks": {} });
    open();
    expect(await screen.findByText("No disks found yet")).toBeTruthy();
    expect(
      (
        screen.getByRole("button", {
          name: /Add virtual disk/,
        }) as HTMLButtonElement
      ).disabled,
    ).toBe(true);
  });

  it("groups disks by machine and puts the ones gone apart", async () => {
    serve({
      "GET /api/disks": {
        node2: [
          disk({
            id: "b",
            model: "",
            device: "/dev/sdb",
            osd_id: null,
            phase: "",
            is_our_osd: false,
            desired: "OFF",
          }),
        ],
        node1: [
          disk(),
          disk({ id: "gone1", desired: "OFF", connected: false }),
          disk({
            id: "gone2",
            desired: "OFF",
            connected: false,
            model: "",
            device: "",
          }),
        ],
      },
    });
    open();
    const disks = section("Disks");
    expect(await within(disks).findByText("node1")).toBeTruthy();
    expect(within(disks).getByText("node2")).toBeTruthy();
    expect(within(disks).getByText("/dev/sdb")).toBeTruthy();
    expect(within(disks).getByText("Connected, not in use")).toBeTruthy();
    expect(within(disks).getByText(/20%/).parentElement!.textContent).toBe(
      " · 20% full",
    );
    fireEvent.click(
      within(disks).getByRole("button", {
        name: /2 disks seen before but not connected/,
      }),
    );
    expect(within(disks).getByText("gone2")).toBeTruthy();
    expect(within(disks).getAllByText("Not connected")).toHaveLength(2);
  });

  it("says when none of the disks it has seen are connected", async () => {
    serve({
      "GET /api/disks": { node1: [disk({ desired: "OFF", connected: false })] },
    });
    open();
    expect(
      await screen.findByText(
        "None of the disks this machine has seen are connected right now.",
      ),
    ).toBeTruthy();
    expect(
      screen.getByText("1 disk seen before but not connected"),
    ).toBeTruthy();
  });

  it.each([
    [{ phase: "creating" as const }, "Setting up…", "text-warning"],
    [{ connected: false }, "Missing — not connected", "text-danger"],
    [
      { phase: "draining" as const },
      "Being removed — moving data off · half moved",
      "text-warning",
    ],
    [{ phase: "removable" as const }, "Safe to unplug", "text-fg-muted"],
    [{ foreign_ceph: true }, "Has data from another system", "text-warning"],
    [{ phase: "unknown" as const }, "Checking…", "text-fg-muted"],
    [
      { is_loop: true, phase: "active" as const, osd_id: 9 },
      "In use · built into this machine",
      "text-fg-muted",
    ],
  ])("shows a disk's state", async (over, label, tone) => {
    serve({ "GET /api/disks": { node1: [disk(over)] } });
    open();
    const text = await within(section("Disks")).findByText(
      label.split(" · ")[0],
    );
    const line = text.closest(
      "span.text-fg-muted, span.text-warning, span.text-danger",
    )!;
    expect(line.textContent).toBe(label);
    expect(line.className).toBe(tone);
  });

  it("leaves out the fill of an active disk whose size is unknown", async () => {
    serve({
      "GET /api/storage": overview({ osds: [osd({ size_bytes: 0 })] }),
      "GET /api/disks": { node1: [disk({ size_bytes: 0, model: "" })] },
    });
    open();
    expect(await within(section("Disks")).findByText("/dev/sda")).toBeTruthy();
    expect(within(section("Disks")).queryByText(/full/)).toBeNull();
  });

  it("has no switch for a disk it cannot identify, and says what it knows", async () => {
    serve({
      "GET /api/disks": {
        node1: [disk({ ownership: "unknown", message: "Reading its label" })],
      },
    });
    open();
    expect(
      await screen.findByText("Can’t identify this disk yet"),
    ).toBeTruthy();
    expect(screen.getByText("Reading its label")).toBeTruthy();
    expect(within(section("Disks")).queryByRole("switch")).toBeNull();
  });

  it("starts using a free disk at once, and says why it did not", async () => {
    const free = disk({
      is_our_osd: false,
      desired: "OFF",
      phase: "",
      osd_id: null,
    });
    const server = serve({
      "GET /api/disks": { node1: [free] },
      "PUT /api/disks/node1/d1": { ok: false, error: "disk busy" },
    });
    open();
    const sw = await screen.findByRole("switch", { name: "Use Samsung" });
    fireEvent.click(sw);
    expect(await screen.findByText("disk busy")).toBeTruthy();
    expect(server.sent("PUT", "/api/disks/node1/d1")[0].body).toEqual({
      desired: "ON",
    });
    server.routes["PUT /api/disks/node1/d1"] = reply(
      500,
      '{"error":"no agent"}',
    );
    fireEvent.click(sw);
    expect(await screen.findByText("no agent")).toBeTruthy();
    server.routes["PUT /api/disks/node1/d1"] = () => Promise.reject("x");
    fireEvent.click(sw);
    expect(
      await screen.findByText("The switch did not reach the server."),
    ).toBeTruthy();
    const before = server.sent("GET", "/api/disks").length;
    server.routes["PUT /api/disks/node1/d1"] = { ok: true };
    fireEvent.click(sw);
    await waitFor(() =>
      expect(server.sent("GET", "/api/disks").length).toBeGreaterThan(before),
    );
    await waitFor(() =>
      expect(
        screen.queryByText("The switch did not reach the server."),
      ).toBeNull(),
    );
  });

  it("asks before it stops using a disk, and can be talked out of it", async () => {
    const server = serve({ "PUT /api/disks/node1/d1": { ok: true } });
    open();
    fireEvent.click(
      await screen.findByRole("switch", { name: "Stop using Samsung" }),
    );
    expect(
      screen.getByText(
        "Its data moves to your other disks first. This can take a while.",
      ),
    ).toBeTruthy();
    expect(
      screen.queryByRole("switch", { name: "Stop using Samsung" }),
    ).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(server.sent("PUT", "/api/disks/node1/d1")).toHaveLength(0);
    fireEvent.click(screen.getByRole("switch", { name: "Stop using Samsung" }));
    fireEvent.click(screen.getByRole("button", { name: "Stop using it" }));
    await waitFor(() =>
      expect(server.sent("PUT", "/api/disks/node1/d1")).toHaveLength(1),
    );
    expect(server.sent("PUT", "/api/disks/node1/d1")[0].body).toEqual({
      desired: "OFF",
    });
  });

  it("warns that a foreign disk is erased before it is used", async () => {
    const server = serve({
      "GET /api/disks": {
        node1: [
          disk({ foreign_ceph: true, desired: "OFF", is_our_osd: false }),
        ],
      },
      "PUT /api/disks/node1/d1": { ok: true },
    });
    open();
    fireEvent.click(await screen.findByRole("switch", { name: "Use Samsung" }));
    expect(screen.getByText("Everything on it will be erased.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Erase and use" }));
    await waitFor(() =>
      expect(server.sent("PUT", "/api/disks/node1/d1")).toHaveLength(1),
    );
  });
});

describe("StoragePage, virtual disks", () => {
  it("shows a virtual disk that is still being made, and one being removed", async () => {
    serve({
      "GET /api/vdisks": [
        vdisk({ state: "provisioning" }),
        vdisk({
          id: 2,
          disk_id: "v2",
          node: "node3",
          state: "removing",
          box_type: "bx99",
        }),
      ],
    });
    open();
    const disks = section("Disks");
    expect(
      await within(disks).findByText("Preparing the virtual disk…"),
    ).toBeTruthy();
    expect(
      within(disks).getByText("Finishing up — do not unplug yet"),
    ).toBeTruthy();
    expect(within(disks).getByText("5 TB")).toBeTruthy();
    expect(within(disks).getByText("bx99")).toBeTruthy();
    expect(
      within(disks).getAllByText(/encrypted here, stored online/),
    ).toHaveLength(2);
    expect(within(disks).getByText("node3")).toBeTruthy();
  });

  it("shows a connected virtual disk by its real size and says what went wrong", async () => {
    serve({
      "GET /api/disks": { node1: [disk({ id: "v1", osd_id: null })] },
      "GET /api/vdisks": [vdisk({ state: "error", status: "box quota" })],
    });
    open();
    const disks = section("Disks");
    expect(await within(disks).findByText("box quota")).toBeTruthy();
    expect(within(disks).getByText("Virtual disk")).toBeTruthy();
    expect(within(disks).queryByText("5 TB")).toBeNull();
  });

  it("says an offline virtual disk is lost for now, and explains removing one", async () => {
    serve({
      "GET /api/disks": { node1: [disk({ id: "v1" })] },
      "GET /api/vdisks": [vdisk({ state: "offline" })],
    });
    open();
    expect(
      await screen.findByText("Offline — its online storage is not answering"),
    ).toBeTruthy();
    fireEvent.click(
      screen.getByRole("switch", { name: "Stop using Virtual disk" }),
    );
    expect(
      screen.getByText(/the virtual disk is deleted and its billing stops/),
    ).toBeTruthy();
  });

  it("adds a virtual disk of the chosen size through the chosen machine", async () => {
    const server = serve({
      "GET /api/disks": { node1: [disk()], node2: [] },
      "POST /api/vdisks": vdisk(),
    });
    open();
    fireEvent.click(
      await screen.findByRole("button", { name: /Add virtual disk/ }),
    );
    expect(await screen.findByText("€3.90/mo")).toBeTruthy();
    expect(screen.getAllByText("…")).toHaveLength(3);
    fireEvent.click(screen.getByRole("button", { name: /^10 TB/ }));
    expect(
      screen
        .getByRole("button", { name: /^10 TB/ })
        .getAttribute("aria-pressed"),
    ).toBe("true");
    fireEvent.change(
      screen.getByRole("combobox", { name: /Connected through/ }),
      { target: { value: "node2" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Add 10 TB" }));
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Add 10 TB" })).toBeNull(),
    );
    expect(server.sent("POST", "/api/vdisks")[0].body).toEqual({
      node: "node2",
      box_type: "bx31",
    });
  });

  it("says why a virtual disk was not added, or its prices could not be read, and can be put away", async () => {
    const server = serve({
      "POST /api/vdisks": reply(402, '{"error":"not enough credit"}'),
      "GET /api/vdisks/offers": reply(502, '{"error":"platform away"}'),
    });
    open();
    fireEvent.click(
      await screen.findByRole("button", { name: /Add virtual disk/ }),
    );
    expect(await screen.findByText("platform away")).toBeTruthy();
    expect(
      screen.queryByRole("combobox", { name: /Connected through/ }),
    ).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Add 1 TB" }));
    expect(await screen.findByText("not enough credit")).toBeTruthy();
    expect(server.sent("POST", "/api/vdisks")[0].body).toEqual({
      node: "node1",
      box_type: "bx11",
    });
    server.routes["POST /api/vdisks"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Add 1 TB" }));
    expect(await screen.findByText("The server did not answer.")).toBeTruthy();
    fireEvent.click(
      within(section("Disks")).getByRole("button", { name: "Cancel" }),
    );
    expect(
      await screen.findByRole("button", { name: /Add virtual disk/ }),
    ).toBeTruthy();
  });
});

describe("StoragePage, protection", () => {
  const copies = () =>
    screen.getByRole("combobox", { name: "Copies of everything" });
  const where = () => screen.getByRole("combobox", { name: "Keep copies on" });

  it("shows the chosen copies and what is available", async () => {
    serve();
    open();
    expect(await screen.findByText("Survives 1 disk failing")).toBeTruthy();
    expect(screen.getByText("2 disks on 1 machine available")).toBeTruthy();
    expect((copies() as HTMLSelectElement).options).toHaveLength(3);
  });

  it("takes the running target when nothing was chosen yet", async () => {
    serve({
      "GET /api/storage/policy": policy({
        policy: null,
        target: {
          size: 3,
          min_size: 1,
          failure_domain: "host",
          mon: 1,
          mgr: 1,
        },
      }),
      "GET /api/storage": overview({
        osds: [
          osd(),
          osd({ id: 1, host: "node2" }),
          osd({ id: 2, host: "node3", weight: 0 }),
        ],
      }),
    });
    open();
    expect(await screen.findByText("Survives 2 machines failing")).toBeTruthy();
    expect((where() as HTMLSelectElement).value).toBe("host");
    expect(screen.getByText("2 disks on 2 machines available")).toBeTruthy();
  });

  it("falls back to one copy on different disks, and warns that it is stored once", async () => {
    serve({
      "GET /api/storage/policy": policy({ policy: null, target: null }),
      "GET /api/storage": overview({ osds: [osd()] }),
    });
    open();
    expect(
      await screen.findByText("No protection against a failed disk"),
    ).toBeTruthy();
    expect(screen.getByText("1 disk on 1 machine available")).toBeTruthy();
    expect((where() as HTMLSelectElement).value).toBe("osd");
  });

  it("warns that a single copy can be lost with its machine", async () => {
    serve({
      "GET /api/storage/policy": policy({
        policy: { size: 1, failure_domain: "host" },
        target: {
          size: 1,
          min_size: 1,
          failure_domain: "host",
          mon: 1,
          mgr: 1,
        },
      }),
    });
    open();
    expect(
      await screen.findByText(/If a machine fails, what was on it is gone/),
    ).toBeTruthy();
  });

  it("shows what a change costs, and applies it", async () => {
    const server = serve({ "PUT /api/storage/policy": { ok: true } });
    open();
    await screen.findByText("Survives 1 disk failing");
    fireEvent.change(copies(), { target: { value: "1" } });
    expect(
      screen.getByText("Keep 1 copy of everything on different disks?"),
    ).toBeTruthy();
    expect(screen.getByText(/would be free afterwards\.$/)).toBeTruthy();
    expect(screen.getByText(/would be free afterwards/).className).toContain(
      "text-fg-muted",
    );
    fireEvent.change(copies(), { target: { value: "3" } });
    expect(
      screen.getByText(
        /You have 2 disks, and each copy needs its own\. YoLab keeps 2 for now and makes the rest when you add a disk\./,
      ),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Apply change" }));
    await waitFor(() =>
      expect(server.sent("PUT", "/api/storage/policy")).toHaveLength(1),
    );
    expect(server.sent("PUT", "/api/storage/policy")[0].body).toEqual({
      size: 3,
      failure_domain: "osd",
    });
  });

  it("says when the copies would be tight or would not fit", async () => {
    const tight = {
      ...overview().space!,
      free_bytes: 0.9e11,
      apps_bytes: 1.5e11,
      images_bytes: 0,
    };
    serve({
      "GET /api/storage": overview({
        osds: [
          osd(),
          osd({ id: 1, host: "node2" }),
          osd({ id: 2, host: "node3" }),
        ],
        space: tight,
      }),
    });
    open();
    await screen.findByText("Survives 1 disk failing");
    fireEvent.change(copies(), { target: { value: "3" } });
    const est = screen.getByText(/would be free afterwards/);
    expect(est.className).toContain("text-warning");
    expect(est.textContent).toMatch(
      /the extra copies write about .* in the background\. It fits, but you will be close to full/,
    );
    fireEvent.click(
      within(section("Protection")).getByRole("button", { name: "Cancel" }),
    );
    expect(screen.queryByText(/would be free afterwards/)).toBeNull();
  });

  it("refuses a change that does not fit", async () => {
    serve({
      "GET /api/storage": overview({
        osds: [osd(), osd({ id: 1 }), osd({ id: 2 })],
        space: { ...overview().space!, free_bytes: 1e9 },
      }),
    });
    open();
    await screen.findByText("Survives 1 disk failing");
    fireEvent.change(copies(), { target: { value: "3" } });
    expect(
      screen.getByText(/Not enough room: the extra copies need about/)
        .className,
    ).toContain("text-danger");
    expect(
      (
        screen.getByRole("button", {
          name: "Apply change",
        }) as HTMLButtonElement
      ).disabled,
    ).toBe(true);
  });

  it("speaks of machines when copies go on different machines", async () => {
    serve();
    open();
    await screen.findByText("Survives 1 disk failing");
    fireEvent.change(where(), { target: { value: "host" } });
    expect(screen.getByText("Survives 1 machine failing")).toBeTruthy();
    expect(
      screen.getByText("Keep 2 copies of everything on different machines?"),
    ).toBeTruthy();
    expect(
      screen.getByText(
        /You have 1 machine, and each copy needs its own\. YoLab keeps 1 for now and makes the rest when you add a machine\./,
      ),
    ).toBeTruthy();
  });

  it("estimates nothing while storage has no space figures yet", async () => {
    serve({ "GET /api/storage": overview({ space: null }) });
    open();
    await screen.findByText("Survives 1 disk failing");
    fireEvent.change(copies(), { target: { value: "1" } });
    expect(screen.queryByText(/would be free afterwards/)).toBeNull();
  });

  it("says why a change was not applied, and cancelling forgets it", async () => {
    const server = serve({
      "PUT /api/storage/policy": reply(
        400,
        '{"error":"keep at least one copy"}',
      ),
    });
    open();
    await screen.findByText("Survives 1 disk failing");
    fireEvent.change(copies(), { target: { value: "3" } });
    fireEvent.click(screen.getByRole("button", { name: "Apply change" }));
    expect(await screen.findByText("keep at least one copy")).toBeTruthy();
    server.routes["PUT /api/storage/policy"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Apply change" }));
    expect(await screen.findByText("The server did not answer.")).toBeTruthy();
    fireEvent.click(
      within(section("Protection")).getByRole("button", { name: "Cancel" }),
    );
    expect(screen.getByText("Survives 1 disk failing")).toBeTruthy();
    expect(screen.queryByText("The server did not answer.")).toBeNull();
  });
});

describe("StoragePage, technical details", () => {
  const details = async () =>
    fireEvent.click(
      await screen.findByRole("button", { name: /Technical details/ }),
    );

  it("shows ceph status, every disk and pool, and the dashboard login", async () => {
    const server = serve({
      "GET /api/storage": overview({
        osds: [
          osd(),
          osd({
            id: 1,
            name: "osd.1",
            host: "",
            class: "",
            up: false,
            weight: 0,
            utilization: 90,
          }),
        ],
        pools: [
          {
            id: 1,
            name: "rbd",
            copies: 2,
            min_copies: 1,
            stored_bytes: 1e9,
            used_bytes: 2e9,
            max_avail_bytes: 3e11,
          },
        ],
      }),
    });
    open();
    await details();
    expect(await screen.findByText("HEALTH_OK all good")).toBeTruthy();
    expect(await screen.findByText("yes")).toBeTruthy();
    const rows = [...document.querySelectorAll("tbody tr")].map((r) =>
      [...r.querySelectorAll("td")].map((c) => c.textContent),
    );
    expect(rows[0]).toEqual(
      expect.arrayContaining([
        "osd.0",
        "node1",
        "ssd",
        "20.0%",
        "32",
        "1.00",
        "up",
        "in",
        "yes",
        "no",
      ]),
    );
    expect(rows[1]).toEqual(
      expect.arrayContaining(["osd.1", "—", "down", "out", "no", "no"]),
    );
    expect(rows[2]).toEqual(expect.arrayContaining(["rbd", "2", "1"]));
    expect(await screen.findByText("admin")).toBeTruthy();
    expect(screen.getByText("••••••••••••")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("s3cret")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Hide password" }));
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(
      screen
        .getByRole("link", { name: /Open Ceph dashboard/ })
        .getAttribute("href"),
    ).toBe("/ceph-dashboard/");
    const before = server.sent("GET", "/api/storage/checks").length;
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() =>
      expect(server.sent("GET", "/api/storage/checks").length).toBeGreaterThan(
        before,
      ),
    );
    expect(
      server.sent("GET", "/api/storage/ceph-status").length,
    ).toBeGreaterThan(1);
  });

  it("says when ceph is still being asked, or the checks are not known yet", async () => {
    serve({
      "GET /api/storage/ceph-status": never(),
      "GET /api/storage/checks": never(),
      "GET /api/ceph/dashboard": never(),
    });
    open();
    await details();
    expect(await screen.findByText("Asking Ceph…")).toBeTruthy();
    expect(screen.getAllByText("…").length).toBeGreaterThanOrEqual(4);
    expect(
      screen.queryByRole("button", { name: /Copy ceph status/ }),
    ).toBeNull();
  });

  it("says why ceph and the safety checks could not be asked", async () => {
    serve({
      "GET /api/storage/ceph-status": reply(500, '{"error":"no keyring"}'),
      "GET /api/storage/checks": reply(500, '{"error":"timeout"}'),
      "GET /api/ceph/dashboard": reply(404, ""),
    });
    open();
    await details();
    expect(await screen.findByText("no keyring")).toBeTruthy();
    expect(
      await screen.findByText("Safety checks unavailable: timeout"),
    ).toBeTruthy();
    expect(screen.queryByText("Ceph dashboard user")).toBeNull();
  });

  it("shows no tables before there are disks or pools", async () => {
    serve({ "GET /api/storage": overview({ osds: [] }) });
    open();
    await details();
    await screen.findByText("HEALTH_OK all good");
    expect(screen.queryByText("Disks (OSDs)")).toBeNull();
    expect(screen.queryByText("Pools")).toBeNull();
  });
});

describe("StoragePage, empty disks", () => {
  it("draws an empty bar when nothing is used", async () => {
    serve({
      "GET /api/storage": overview({
        raw: { total_bytes: 0, used_bytes: 0, avail_bytes: 0, data_bytes: 0 },
      }),
    });
    open();
    await screen.findByText("free for your files");
    const fill = document.querySelector(
      ".h-2.overflow-hidden > div",
    ) as HTMLElement;
    expect(fill.style.width).toBe("0%");
  });
});
