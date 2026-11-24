import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ForceHealCard, HealBanner } from "./ForceHeal";
import { fakeServer, reply, settle } from "@/test/server";
import { renderAt } from "@/test/render";
import type { Heal, HealStatus, MachineReset } from "@/lib/heal";

type Machine = HealStatus["survey"]["machines"][number];

const machine = (over: Partial<Machine> = {}): Machine => ({
  label: "node1",
  name: "node1",
  addr: "fd00::1",
  this_machine: false,
  answers: true,
  reset: null,
  ...over,
});

function status(
  over: Partial<HealStatus> = {},
  survey: Partial<HealStatus["survey"]> = {},
): HealStatus {
  return {
    survey: {
      me: "node2",
      machines: [
        machine(),
        machine({ label: "node2", name: "node2", this_machine: true }),
      ],
      unreadable: [],
      ceph_quorum: true,
      kubernetes: true,
      uptime_secs: 100,
      lost_groups: 0,
      ...survey,
    },
    problems: [],
    refusal: null,
    plan: { keep_machines: ["node2"], remove_machines: [] },
    heal: null,
    ...over,
  };
}

const nowSecs = () => Math.floor(Date.now() / 1000);

function heal(over: Partial<Heal> = {}): Heal {
  return {
    id: "h1",
    driver: "node2",
    running: true,
    failed: null,
    started_at: nowSecs() - 100,
    finished_at: null,
    step: "restart",
    steps: ["prepare", "arm", "restart", "rebuild"],
    members: ["node2"],
    removed_machines: [],
    waiting: null,
    ...over,
  };
}

const reset = (phase: MachineReset["phase"]): MachineReset => ({
  heal_id: "h1",
  driver: "node3",
  phase,
  error: null,
});

function serve(s: HealStatus) {
  return fakeServer({ "GET /api/heal": s, "POST /api/heal": undefined });
}

describe("HealBanner", () => {
  it("shows nothing before it knows, or when all is well", async () => {
    serve(status());
    const { container } = renderAt(<HealBanner />);
    await settle();
    expect(container.querySelector("[role]")).toBeNull();
  });

  it("follows a running heal", async () => {
    serve(status({ heal: heal() }));
    renderAt(<HealBanner className="b" />);
    expect(
      await screen.findByText("Your home server is being healed"),
    ).toBeTruthy();
    expect(screen.getByText(/Restarting every machine\./)).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "See progress" }));
    expect(screen.getByTestId("where").textContent).toBe("/system/storage");
  });

  it("says another machine is healing this one", async () => {
    serve(
      status(
        {},
        { machines: [machine({ this_machine: true, reset: reset("armed") })] },
      ),
    );
    renderAt(<HealBanner />);
    expect(
      await screen.findByText(/node3 is rebuilding the cluster/),
    ).toBeTruthy();
    expect(screen.queryByRole("link")).toBeNull();
  });

  it("reports a heal that just finished or was stopped", async () => {
    serve(
      status({ heal: heal({ running: false, finished_at: nowSecs() - 10 }) }),
    );
    const first = renderAt(<HealBanner />);
    expect(await screen.findByText("Your home server was healed")).toBeTruthy();
    expect(screen.getByText(/Add from backup/)).toBeTruthy();
    first.unmount();
    serve(
      status({
        heal: heal({
          running: false,
          failed: "node1 failed",
          finished_at: nowSecs() - 10,
        }),
      }),
    );
    renderAt(<HealBanner />);
    expect(await screen.findByText("The heal was stopped")).toBeTruthy();
    expect(
      screen.getByText("Every machine was put back as it was."),
    ).toBeTruthy();
    expect(screen.getByRole("link", { name: "Details" })).toBeTruthy();
  });

  it("names the first problem", async () => {
    serve(status({ problems: ["ceph_no_quorum", "machines_gone"] }));
    renderAt(<HealBanner />);
    expect(await screen.findByText("Storage has lost its quorum")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Look at it" })).toBeTruthy();
  });
});

describe("ForceHealCard", () => {
  it("shows nothing when all is well, or long after a heal", async () => {
    serve(status({ heal: heal({ running: false, finished_at: 1000 }) }));
    const { container } = renderAt(<ForceHealCard />);
    await settle();
    expect(container.querySelector("ol, button")).toBeNull();
  });

  it("follows a running heal step by step", async () => {
    serve(status({ heal: heal({ waiting: "Waiting for node1" }) }));
    const { container } = renderAt(<ForceHealCard />);
    expect(
      await screen.findByText("Healing the cluster from node2"),
    ).toBeTruthy();
    expect(screen.getByText(/This page updates by itself/)).toBeTruthy();
    const items = [...container.querySelectorAll("ol li p")];
    expect(items.map((p) => p.className.includes("text-fg-muted"))).toEqual([
      true,
      true,
      false,
      true,
      false,
    ]);
    expect(items[2].className).toContain("font-medium");
    expect(items[3].textContent).toBe("Waiting for node1");
    expect(items[4].className).toContain("text-fg-subtle");
    expect(screen.getByText("Waiting for node1")).toBeTruthy();
    expect(container.querySelector(".animate-spin")).not.toBeNull();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("shows a stop in progress as its own single step", async () => {
    serve(
      status({
        heal: heal({ step: "undo", failed: "node1 could not prepare" }),
      }),
    );
    const { container } = renderAt(<ForceHealCard />);
    expect(await screen.findByText("Stopping the heal")).toBeTruthy();
    expect(
      screen.getByText(/node1 could not prepare\. Nothing was deleted/),
    ).toBeTruthy();
    expect(container.querySelectorAll("ol li")).toHaveLength(1);
  });

  it("offers to heal again when a heal is stuck rebuilding with problems", async () => {
    serve(
      status({
        heal: heal({ step: "rebuild" }),
        problems: ["kubernetes_down"],
      }),
    );
    renderAt(<ForceHealCard />);
    fireEvent.click(
      await screen.findByRole("button", { name: "FORCE HEAL again" }),
    );
    expect(screen.getByText("Force heal the cluster?")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByText("Force heal the cluster?")).toBeNull();
  });

  it("does not offer to heal again when refused, or not stuck", async () => {
    serve(
      status({
        heal: heal({ step: "undo" }),
        problems: ["kubernetes_down"],
        refusal: "No",
      }),
    );
    const first = renderAt(<ForceHealCard />);
    await screen.findByText("Healing the cluster from node2");
    expect(screen.queryByRole("button")).toBeNull();
    first.unmount();
    serve(status({ heal: heal({ step: "rebuild" }) }));
    renderAt(<ForceHealCard />);
    await screen.findByText("Healing the cluster from node2");
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("says when another machine is healing this one", async () => {
    serve(
      status(
        {},
        {
          machines: [
            machine({ this_machine: true, reset: reset("preparing") }),
          ],
        },
      ),
    );
    renderAt(<ForceHealCard />);
    expect(
      await screen.findByText("node3 is healing the cluster"),
    ).toBeTruthy();
  });

  it("shows a heal that just finished, with what was left out", async () => {
    serve(
      status({
        heal: heal({
          running: false,
          finished_at: nowSecs() - 5,
          removed_machines: ["node1", "node3"],
        }),
      }),
    );
    renderAt(<ForceHealCard />);
    expect(await screen.findByText("The cluster was healed")).toBeTruthy();
    expect(screen.getByText("node1, node3")).toBeTruthy();
    fireEvent.click(
      screen.getByRole("link", { name: "Add services from backup" }),
    );
    expect(screen.getByTestId("where").textContent).toBe("/");
  });

  it("shows a stopped heal without offering backups", async () => {
    serve(
      status({
        heal: heal({
          running: false,
          failed: "x",
          finished_at: nowSecs() - 5,
          removed_machines: ["node1"],
        }),
      }),
    );
    renderAt(<ForceHealCard />);
    expect(await screen.findByText("The heal was stopped")).toBeTruthy();
    expect(screen.queryByText(/Left out/)).toBeNull();
    expect(screen.queryByRole("link")).toBeNull();
  });

  it("lists the problems, who is gone and what is lost, and refuses when told to", async () => {
    serve(
      status(
        {
          problems: ["machines_gone", "data_unreachable"],
          refusal: "node1 might come back",
        },
        {
          machines: [
            machine({ answers: false }),
            machine({ label: "node3", answers: false }),
          ],
          lost_groups: 1,
        },
      ),
    );
    renderAt(<ForceHealCard />);
    expect(await screen.findByText("A machine does not answer")).toBeTruthy();
    expect(screen.getByText("node1, node3")).toBeTruthy();
    expect(screen.getByText(/1 group of files have no copy/)).toBeTruthy();
    expect(screen.getByText("node1 might come back")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "FORCE HEAL" })).toBeNull();
  });

  it("counts lost groups in the plural, and says nothing of them when unknown or none", async () => {
    serve(status({ problems: ["ceph_no_quorum"] }, { lost_groups: 3 }));
    const first = renderAt(<ForceHealCard />);
    expect(await screen.findByText(/3 groups of files/)).toBeTruthy();
    expect(screen.queryByText(/Not answering/)).toBeNull();
    first.unmount();
    serve(status({ problems: ["ceph_no_quorum"] }, { lost_groups: null }));
    renderAt(<ForceHealCard />);
    await screen.findByText("Storage has lost its quorum");
    expect(screen.queryByText(/groups? of files/)).toBeNull();
  });

  it("heals only after every gone machine and HEAL are typed", async () => {
    const s = status(
      {
        problems: ["machines_gone"],
        plan: { keep_machines: ["node2"], remove_machines: ["node1", "node3"] },
      },
      { unreadable: ["node1: timeout"] },
    );
    const server = serve(s);
    renderAt(<ForceHealCard />);
    fireEvent.click(await screen.findByRole("button", { name: "FORCE HEAL" }));
    expect(screen.getByText(/Could not read: node1: timeout/)).toBeTruthy();
    expect(screen.getAllByText("node1, node3").length).toBeGreaterThan(0);
    const go = screen.getByRole("button", {
      name: "Erase everything and heal",
    });
    expect(go).toHaveProperty("disabled", true);
    fireEvent.change(screen.getByLabelText("Type HEAL to continue"), {
      target: { value: " HEAL " },
    });
    fireEvent.change(screen.getByLabelText("Type node1 to confirm"), {
      target: { value: "node1" },
    });
    expect(go).toHaveProperty("disabled", true);
    fireEvent.change(screen.getByLabelText("Type node3 to confirm"), {
      target: { value: "node3 " },
    });
    expect(go).toHaveProperty("disabled", false);
    fireEvent.click(go);
    await waitFor(() =>
      expect(screen.queryByText("Force heal the cluster?")).toBeNull(),
    );
    expect(server.sent("POST", "/api/heal")[0].body).toEqual({
      keep_machines: ["node2"],
      remove_machines: ["node1", "node3"],
    });
    await waitFor(() =>
      expect(server.sent("GET", "/api/heal").length).toBeGreaterThan(1),
    );
  });

  it("says why the heal did not start", async () => {
    const server = serve(status({ problems: ["kubernetes_down"] }));
    server.routes["POST /api/heal"] = reply(409, '{"error":"already healing"}');
    renderAt(<ForceHealCard />);
    fireEvent.click(await screen.findByRole("button", { name: "FORCE HEAL" }));
    expect(screen.queryByLabelText(/to confirm/)).toBeNull();
    expect(screen.queryByText(/Left out for good/)).toBeNull();
    expect(screen.queryByText(/Could not read/)).toBeNull();
    fireEvent.change(screen.getByLabelText("Type HEAL to continue"), {
      target: { value: "HEAL" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Erase everything and heal" }),
    );
    expect(await screen.findByText("already healing")).toBeTruthy();
    server.routes["POST /api/heal"] = () => Promise.reject("x");
    fireEvent.click(
      screen.getByRole("button", { name: "Erase everything and heal" }),
    );
    expect(await screen.findByText("Could not start the heal")).toBeTruthy();
  });

  it("starts a second heal from a stuck one and refreshes", async () => {
    const server = serve(
      status({
        heal: heal({ step: "rebuild" }),
        problems: ["kubernetes_down"],
      }),
    );
    renderAt(<ForceHealCard />);
    fireEvent.click(
      await screen.findByRole("button", { name: "FORCE HEAL again" }),
    );
    fireEvent.change(screen.getByLabelText("Type HEAL to continue"), {
      target: { value: "HEAL" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Erase everything and heal" }),
    );
    await waitFor(() =>
      expect(server.sent("GET", "/api/heal").length).toBeGreaterThan(1),
    );
  });
});
