import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Outlet, Route, Routes } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import { SystemPage } from "./SystemPage";
import { NodesPage } from "./NodesPage";
import { fakeServer, never, reply } from "@/test/server";
import { Where } from "@/test/Where";
import type { NodeInfo } from "@/types/nodes";
import { remember } from "@/lib/localCache";

const health = (level: string, starting = false) => ({
  level,
  title: "",
  message: "",
  issues: [],
  starting,
  provisioning: false,
});

const settled = { state: "settled", jobs: [] };

function routes(over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/cluster/health": health("ok"),
    "GET /api/storage/movement": settled,
    "GET /api/storage": never(),
    "GET /api/nodes": [],
    "GET /api/status": { commit_hash: "abc", commit_date: "" },
    "GET /api/backups/s3": never(),
    "GET /api/store/settings": never(),
    ...over,
  });
}

function page(signOut = vi.fn()) {
  render(
    <MemoryRouter initialEntries={["/system"]}>
      <Routes>
        <Route element={<Outlet context={{ signOut }} />}>
          <Route path="*" element={<SystemPage />} />
        </Route>
      </Routes>
      <Where />
    </MemoryRouter>,
  );
  return signOut;
}

const row = (label: string) =>
  screen
    .getByText(label, { selector: "div.text-sm.font-medium" })
    .closest("a, button") as HTMLElement;

describe("SystemPage", () => {
  it("links to each part of the system", async () => {
    routes();
    page();
    for (const [label, to] of [
      ["Storage", "/system/storage"],
      ["Backups", "/system/backups"],
      ["Machines", "/system/machines"],
      ["Updates", "/system/updates"],
      ["Settings", "/system/settings"],
      ["Logs", "/system/logs"],
    ]) {
      expect(row(label).getAttribute("href"), label).toBe(to);
    }
    fireEvent.click(row("Updates"));
    expect(screen.getByTestId("where").textContent).toBe("/system/updates");
    expect(screen.queryByText("Account and billing")).toBeNull();
    expect(screen.queryByText("Change server")).toBeNull();
  });

  it("shows free space, or that storage is starting, coloured by health", async () => {
    routes({
      "GET /api/storage": { space: { free_bytes: 5e11 } },
      "GET /api/cluster/health": health("error"),
    });
    page();
    expect(await screen.findByText(/free/)).toBeTruthy();
    await waitFor(() =>
      expect(row("Storage").innerHTML).toContain("bg-danger-soft"),
    );
  });

  it("says storage is starting, and warns", async () => {
    routes({ "GET /api/cluster/health": health("warn", true) });
    page();
    expect(await screen.findByText("Starting up…")).toBeTruthy();
    expect(row("Storage").innerHTML).toContain("bg-warning-soft");
  });

  it("summarises data being moved instead of free space", async () => {
    routes({
      "GET /api/storage/movement": {
        state: "working",
        jobs: [],
        move_reason: null,
        draining: [],
        filling: [],
        waiting_for: [],
        copies: null,
        clones: 0,
        repairing: false,
        eta_secs: null,
      },
      "GET /api/storage": { space: { free_bytes: 1 } },
    });
    page();
    expect(
      await screen.findByText("Rearranging your files across your disks"),
    ).toBeTruthy();
  });

  it("says whether backups are on, warning when they are not", async () => {
    const server = routes({ "GET /api/backups/s3": { configured: true } });
    const first = page;
    first();
    expect(await screen.findByText("Turned on")).toBeTruthy();
    expect(server.calls.length).toBeGreaterThan(0);
  });

  it("warns that files are not backed up", async () => {
    routes({ "GET /api/backups/s3": { configured: false } });
    page();
    expect(
      await screen.findByText(/your files are not backed up/),
    ).toBeTruthy();
    expect(row("Backups").innerHTML).toContain("bg-warning-soft");
  });

  it("counts the machines", async () => {
    routes({ "GET /api/nodes": [{ name: "a" }] });
    page();
    expect(await screen.findByText("machine")).toBeTruthy();
  });

  it("counts several machines", async () => {
    routes({ "GET /api/nodes": [{ name: "a" }, { name: "b" }] });
    page();
    expect(await screen.findByText("machines")).toBeTruthy();
  });

  it("shows the account's credit and opens the console in a new tab", async () => {
    const tab = { opener: {}, location: { replace: vi.fn() }, close: vi.fn() };
    vi.spyOn(window, "open").mockReturnValue(tab as unknown as Window);
    routes({
      "GET /api/status": {
        commit_hash: "",
        commit_date: "",
        console_url: "https://console",
      },
      "GET /api/billing/balance": {
        credit_cents: 1234,
        suspended: true,
        stops_on: null,
      },
      "GET /api/console/link": { url: "https://console/signed" },
    });
    page();
    expect(
      await screen.findByText(/YoLab tunnels and backups are stopped/),
    ).toBeTruthy();
    expect(row("Account and billing").innerHTML).toContain("bg-danger-soft");
    fireEvent.click(row("Account and billing"));
    await waitFor(() =>
      expect(tab.location.replace).toHaveBeenCalledWith(
        "https://console/signed",
      ),
    );
    expect(tab.opener).toBeNull();
  });

  it("before the credit is known it says what the account page holds", async () => {
    routes({
      "GET /api/status": {
        commit_hash: "",
        commit_date: "",
        console_url: "https://console",
      },
      "GET /api/billing/balance": never(),
    });
    page();
    expect(
      await screen.findByText("Your credit, top-ups and referral code"),
    ).toBeTruthy();
  });

  it("falls back to the plain console address, or to this tab when popups are blocked", async () => {
    const assign = vi.fn();
    const real = window.location;
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { ...real, assign },
    });
    vi.spyOn(window, "open").mockReturnValue(null);
    try {
      routes({
        "GET /api/status": {
          commit_hash: "",
          commit_date: "",
          console_url: "https://console",
        },
        "GET /api/billing/balance": never(),
        "GET /api/console/link": reply(502, "down"),
      });
      page();
      fireEvent.click(await screen.findByText("Account and billing"));
      await waitFor(() =>
        expect(assign).toHaveBeenCalledWith("https://console"),
      );
    } finally {
      Object.defineProperty(window, "location", {
        configurable: true,
        value: real,
      });
    }
  });

  it("signs out, and offers to change server inside the app", async () => {
    routes();
    const real = window.location;
    const fake = { href: "https://x" };
    (window as unknown as { __YOLAB_SHELL__: boolean }).__YOLAB_SHELL__ = true;
    try {
      const signOut = page();
      fireEvent.click(row("Sign out"));
      expect(signOut).toHaveBeenCalledOnce();
      Object.defineProperty(window, "location", {
        configurable: true,
        value: fake,
      });
      fireEvent.click(row("Change server"));
      expect(fake.href).not.toBe("https://x");
    } finally {
      delete (window as unknown as { __YOLAB_SHELL__?: boolean })
        .__YOLAB_SHELL__;
      Object.defineProperty(window, "location", {
        configurable: true,
        value: real,
      });
    }
  });
});

const node = (over: Partial<NodeInfo> = {}): NodeInfo => ({
  name: "node1",
  ip: "fd00::1",
  ready: true,
  roles: [],
  joined_at: "2026-01-02T00:00:00Z",
  ...over,
});

describe("NodesPage", () => {
  function serve(nodes: unknown, links: unknown = [], paths: unknown = []) {
    return fakeServer({
      "GET /api/nodes": nodes,
      "GET /api/nodes/links": links,
      "GET /api/mesh/paths": paths,
    });
  }

  it("is loading, then says when there are no machines", async () => {
    serve([]);
    render(<NodesPage />);
    expect(screen.getByText("Loading…")).toBeTruthy();
    expect(await screen.findByText("No machines found.")).toBeTruthy();
    expect(screen.getByText(/of/).textContent).toContain("ready");
  });

  it("lists each machine with its status, hardware, join date, path and link", async () => {
    serve(
      [
        node({
          hardware: {
            accelerator: "nvidia",
            vram_gib: 8,
            ram_gib: 32,
            game_input: false,
            arch: "x86_64",
          },
        }),
        node({
          name: "node2",
          ip: "fd00::2",
          ready: false,
          joined_at: "",
          hardware: {
            accelerator: "cpu",
            vram_gib: null,
            ram_gib: null,
            game_input: false,
          },
        }),
        node({
          name: "node3",
          ip: "fd00::3",
          hardware: {
            accelerator: null,
            vram_gib: null,
            ram_gib: null,
            game_input: false,
          },
        }),
      ],
      [{ name: "node1", url: "https://node1.example" }],
      [
        {
          node: "fd00::1",
          path: "direct",
          endpoint: "1.2.3.4:51820",
          handshake_age_secs: 1,
        },
        {
          node: "fd00::2",
          path: "relayed",
          endpoint: null,
          handshake_age_secs: 1,
        },
        {
          node: "fd00::3",
          path: "direct",
          endpoint: null,
          handshake_age_secs: 1,
        },
      ],
    );
    render(<NodesPage />);
    expect(await screen.findAllByText("Ready")).toHaveLength(2);
    expect(screen.getByText("Not Ready")).toBeTruthy();
    expect(screen.getByText("1.2.3.4:51820")).toBeTruthy();
    expect(screen.getByText("Relayed")).toBeTruthy();
    expect(screen.getAllByText("Direct")).toHaveLength(2);
    expect(screen.getByRole("link").textContent).toBe("node1.example");
    expect(screen.getByText("—", { selector: "td.text-xs" })).toBeTruthy();
    expect(screen.getByText(/ready$/).textContent).toBe("2 of 3 ready");
  });

  it("shows remembered machines as offline while the cluster API is down", async () => {
    remember("nodes", [node()], Date.now());
    serve(reply(503, "down"));
    render(<NodesPage />);
    expect(await screen.findByText("Offline")).toBeTruthy();
    expect(screen.getByText(/Cluster API unreachable/)).toBeTruthy();
    expect(screen.queryByText(/ready$/)).toBeNull();
  });

  it("shows no machines when the API is down and nothing is remembered", async () => {
    serve(reply(503, "down"));
    render(<NodesPage />);
    expect(await screen.findByText("No machines found.")).toBeTruthy();
  });
});
