import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { HomePage } from "./HomePage";
import { fakeServer, never, reply, settle } from "@/test/server";
import { renderAt } from "@/test/render";
import { appInfo, catalogApp } from "@/test/fixtures";

const healthy = {
  level: "ok",
  title: "All good",
  message: "",
  issues: [],
  starting: false,
  provisioning: false,
};

function serve(over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/apps": [],
    "GET /api/apps/catalog": [],
    "GET /api/cluster/health": healthy,
    "GET /api/heal": never(),
    "GET /api/storage/movement": never(),
    ...over,
  });
}

const where = () => screen.getByTestId("where").textContent;

describe("HomePage", () => {
  it("shows placeholder tiles while the services load", () => {
    serve({ "GET /api/apps": never() });
    const { container } = renderAt(<HomePage />);
    expect(
      container.querySelectorAll(".shimmer").length,
    ).toBeGreaterThanOrEqual(8);
  });

  it("offers to try again when the services cannot be read", async () => {
    const server = serve({ "GET /api/apps": reply(502, "down") });
    renderAt(<HomePage />);
    fireEvent.click(await screen.findByRole("button", { name: "Try again" }));
    await waitFor(() =>
      expect(server.sent("GET", "/api/apps")).toHaveLength(2),
    );
  });

  it("invites the first install", async () => {
    serve();
    renderAt(<HomePage />);
    fireEvent.click(
      await screen.findByRole("link", { name: /Browse services/ }),
    );
    expect(where()).toBe("/add");
  });

  it("lays out services as tiles and packs as their own sections", async () => {
    serve({
      "GET /api/apps": [
        appInfo({ app_id: "zz", instance_name: "zz" }),
        appInfo({ app_id: "unknown", instance_name: "mystery" }),
        appInfo({ app_id: "media", instance_name: "media" }),
      ],
      "GET /api/apps/catalog": [
        catalogApp({ id: "zz", name: "Zed", icon: "🐙" }),
        catalogApp({
          id: "media",
          name: "Media pack",
          members: ["jellyfin", "sonarr"],
        }),
        catalogApp({ id: "jellyfin", name: "Jellyfin", icon: "🎬" }),
      ],
    });
    renderAt(<HomePage />);
    expect(await screen.findByText("Media pack")).toBeTruthy();
    expect(screen.getByText("Zed")).toBeTruthy();
    expect(screen.getByText("🐙")).toBeTruthy();
    expect(screen.getAllByText("📦").length).toBeGreaterThan(0);
    expect(screen.getByText("Jellyfin")).toBeTruthy();
    expect(screen.getByText("sonarr")).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "Manage" }));
    expect(where()).toBe("/app/media");
  });

  it("has an Add tile next to the services", async () => {
    serve({ "GET /api/apps": [appInfo()] });
    renderAt(<HomePage />);
    fireEvent.click(await screen.findByRole("link", { name: "Add" }));
    expect(where()).toBe("/add");
  });

  it("says storage is starting, without asking you to look", async () => {
    serve({
      "GET /api/cluster/health": { ...healthy, level: "warn", starting: true },
    });
    renderAt(<HomePage />);
    expect(await screen.findByText("Your storage is starting up")).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Look at storage" })).toBeNull();
  });

  it("says a new disk is being prepared", async () => {
    serve({ "GET /api/cluster/health": { ...healthy, provisioning: true } });
    renderAt(<HomePage />);
    expect(await screen.findByText("Preparing a new disk")).toBeTruthy();
  });

  it("puts the worst issue first and links to the others", async () => {
    serve({
      "GET /api/cluster/health": {
        ...healthy,
        level: "error",
        starting: true,
        issues: [
          { level: "warn", title: "Slow disk", description: "d1" },
          { level: "error", title: "Disk gone", description: "d2" },
        ],
      },
    });
    renderAt(<HomePage />);
    expect(await screen.findByText("Disk gone")).toBeTruthy();
    expect(screen.getByText("d2")).toBeTruthy();
    expect(screen.getByRole("link", { name: "1 other issue" })).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "Look at storage" }));
    expect(where()).toBe("/system/storage");
  });

  it("counts several other issues", async () => {
    const issue = { level: "warn", title: "t", description: "d" };
    serve({
      "GET /api/cluster/health": {
        ...healthy,
        level: "warn",
        issues: [issue, issue, issue],
      },
    });
    renderAt(<HomePage />);
    expect(
      await screen.findByRole("link", { name: "2 other issues" }),
    ).toBeTruthy();
  });

  it("falls back to the overall message when no issue is listed", async () => {
    serve({
      "GET /api/cluster/health": {
        ...healthy,
        level: "warn",
        title: "Degraded",
        message: "Something",
      },
    });
    renderAt(<HomePage />);
    expect(await screen.findByText("Degraded")).toBeTruthy();
    expect(screen.getByText("Something")).toBeTruthy();
    expect(screen.queryByText(/other/)).toBeNull();
  });

  it("shows the heal banner instead of the storage one", async () => {
    serve({
      "GET /api/cluster/health": { ...healthy, level: "error" },
      "GET /api/heal": {
        survey: {
          me: "n",
          machines: [],
          unreadable: [],
          ceph_quorum: false,
          kubernetes: false,
          uptime_secs: 1,
          lost_groups: 0,
        },
        problems: ["ceph_no_quorum"],
        refusal: null,
        plan: { keep_machines: [], remove_machines: [] },
        heal: null,
      },
    });
    renderAt(<HomePage />);
    expect(await screen.findByText("Storage has lost its quorum")).toBeTruthy();
    await settle();
    expect(screen.queryByText("All good")).toBeNull();
  });

  it("shows files that cannot be reached above a storage warning", async () => {
    serve({
      "GET /api/cluster/health": {
        ...healthy,
        level: "warn",
        title: "Degraded",
      },
      "GET /api/storage/movement": {
        state: "no_room",
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
      },
    });
    renderAt(<HomePage />);
    expect(
      await screen.findByText(
        "Not enough room to finish reorganising your files",
      ),
    ).toBeTruthy();
    expect(screen.queryByText("Degraded")).toBeNull();
  });
});
