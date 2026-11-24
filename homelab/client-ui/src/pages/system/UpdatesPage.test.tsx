import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { UpdatesPage } from "./UpdatesPage";
import { LogsPage } from "./LogsPage";
import { fakeServer, never, reply, settle, stream } from "@/test/server";

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
}

const idle = { running: false, log: [] };

function serve(over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/status": {
      commit_hash: "abcdef1234567",
      commit_date: "2026-03-04T05:06:00Z",
    },
    "GET /api/update/channel": never(),
    "GET /api/notifications": never(),
    "GET /api/rebuild-log": idle,
    ...over,
  });
}

const fast = () => vi.useFakeTimers({ shouldAdvanceTime: true });
const tick = () => act(() => vi.advanceTimersByTimeAsync(2000));

describe("UpdatesPage", () => {
  it("shows the running commit and when it was built, and no log before any build", async () => {
    serve();
    render(<UpdatesPage />);
    expect(await screen.findByText("abcdef12")).toBeTruthy();
    expect(screen.getByText("github:DemyCode/yolab / main")).toBeTruthy();
    await settle();
    expect(screen.queryByText("Last build")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("shows dashes for an unknown commit, and the status error", async () => {
    serve({
      "GET /api/status": {
        commit_hash: "",
        commit_date: "",
        error: "git is broken",
      },
    });
    render(<UpdatesPage />);
    expect((await screen.findByRole("alert")).textContent).toBe(
      "git is broken",
    );
    expect(screen.getAllByText("—")).toHaveLength(2);
  });

  it("says why the last build could not be read", async () => {
    serve({ "GET /api/rebuild-log": reply(500, '{"error":"no journal"}') });
    render(<UpdatesPage />);
    expect((await screen.findByRole("alert")).textContent).toBe("no journal");
  });

  it("names the failure when the server did not answer about the last build", async () => {
    serve({ "GET /api/rebuild-log": () => Promise.reject(new Error("x")) });
    render(<UpdatesPage />);
    expect((await screen.findByRole("alert")).textContent).toBe(
      "Could not read the last build",
    );
  });

  it("shows the last build, each kind of line in its own colour", async () => {
    serve({
      "GET /api/rebuild-log": {
        running: false,
        log: [
          "$ nixos-rebuild",
          "warning: w",
          "[INFO] i",
          "x error: e",
          "[ERROR] bad",
          "plain",
        ],
      },
    });
    render(<UpdatesPage />);
    expect(await screen.findByText("Last build")).toBeTruthy();
    expect(screen.getByText("✓ Done")).toBeTruthy();
    const tone = (t: string) => screen.getByText(t).className;
    expect(tone("$ nixos-rebuild")).toContain("text-success");
    expect(tone("warning: w")).toContain("text-warning");
    expect(tone("[INFO] i")).toContain("text-fg-subtle");
    expect(tone("x error: e")).toContain("text-danger");
    expect(tone("[ERROR] bad")).toContain("text-danger");
    expect(tone("plain")).toContain("text-fg-muted");
  });

  it("follows a build already running, through a failed poll, to the end", async () => {
    fast();
    let n = 0;
    const server = serve({
      "GET /api/rebuild-log": () => {
        n += 1;
        if (n === 1) return { running: true, log: ["a"] };
        if (n === 2) return Promise.reject(new Error("restarting"));
        if (n === 3) return { running: true, log: ["a", "b"] };
        return { running: false, log: ["a", "b", "c"] };
      },
    });
    render(<UpdatesPage />);
    expect(await screen.findByText("Building…")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Updating…/ })).toHaveProperty(
      "disabled",
      true,
    );
    await tick();
    await tick();
    expect(screen.getByText("b")).toBeTruthy();
    await tick();
    expect(await screen.findByText("✓ Done")).toBeTruthy();
    expect(screen.getAllByText(/^[abc]$/).map((e) => e.textContent)).toEqual([
      "a",
      "b",
      "c",
    ]);
    await waitFor(() =>
      expect(server.sent("GET", "/api/status").length).toBe(2),
    );
  });

  it("stops following a build when the page goes away", async () => {
    fast();
    let n = 0;
    const later = deferred<unknown>();
    const server = serve({
      "GET /api/rebuild-log": () => {
        n += 1;
        if (n === 1) return { running: true, log: [] };
        if (n === 2) return Promise.reject(new Error("x"));
        return later.promise;
      },
    });
    const view = render(<UpdatesPage />);
    await screen.findByText("Building…");
    await tick();
    await tick();
    view.unmount();
    later.resolve({ running: true, log: ["late"] });
    await tick();
    await tick();
    expect(server.sent("GET", "/api/rebuild-log")).toHaveLength(3);
  });

  it("does not retry after the page went away during a failed poll", async () => {
    fast();
    let n = 0;
    const failing = deferred<unknown>();
    const server = serve({
      "GET /api/rebuild-log": () => {
        n += 1;
        return n === 1 ? { running: true, log: [] } : failing.promise;
      },
    });
    const view = render(<UpdatesPage />);
    await screen.findByText("Building…");
    await tick();
    view.unmount();
    failing.reject(new Error("x"));
    await tick();
    await tick();
    expect(server.sent("GET", "/api/rebuild-log")).toHaveLength(2);
  });

  it("ignores the first answers once the page went away", async () => {
    const ok = deferred<unknown>();
    serve({ "GET /api/rebuild-log": () => ok.promise });
    const a = render(<UpdatesPage />);
    a.unmount();
    ok.resolve({ running: true, log: ["x"] });
    await settle();
    const bad = deferred<unknown>();
    serve({ "GET /api/rebuild-log": () => bad.promise });
    const b = render(<UpdatesPage />);
    b.unmount();
    bad.reject(new Error("x"));
    await settle();
    expect(screen.queryByText("x")).toBeNull();
  });

  it("updates every machine, streaming then following the build", async () => {
    const started = deferred<Response>();
    let rebuild = 0;
    const server = serve({
      "POST /api/update/all": () => started.promise,
      "GET /api/rebuild-log": () => {
        rebuild += 1;
        return rebuild === 1 ? idle : { running: false, log: ["built"] };
      },
    });
    render(<UpdatesPage />);
    await settle();
    fireEvent.click(
      screen.getByRole("button", { name: "Update all machines" }),
    );
    expect(await screen.findByText("Fetching & resetting…")).toBeTruthy();
    started.resolve(stream("$ git fetch", "[DONE]")());
    expect(await screen.findByText("built")).toBeTruthy();
    expect(screen.getByText("$ git fetch")).toBeTruthy();
    expect(server.sent("POST", "/api/update/all")).toHaveLength(1);
  });

  it("writes why an update did not start into the log, once", async () => {
    serve({ "POST /api/update/all": reply(409, '{"error":"busy"}') });
    render(<UpdatesPage />);
    await settle();
    fireEvent.click(
      screen.getByRole("button", { name: "Update all machines" }),
    );
    expect(await screen.findByText("[ERROR] busy")).toBeTruthy();
  });

  it("does not repeat an error the stream already printed", async () => {
    serve({ "POST /api/update/all": stream("[ERROR] git failed") });
    render(<UpdatesPage />);
    await settle();
    fireEvent.click(
      screen.getByRole("button", { name: "Update all machines" }),
    );
    expect(await screen.findByText("[ERROR] git failed")).toBeTruthy();
    await settle();
    expect(screen.getAllByText(/\[ERROR\]/)).toHaveLength(1);
  });

  it("reboots every machine only after a second click", async () => {
    const server = serve({ "POST /api/system/reboot/all": undefined });
    render(<UpdatesPage />);
    fireEvent.click(
      screen.getByRole("button", { name: "Reboot all machines" }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(server.sent("POST", "/api/system/reboot/all")).toHaveLength(0);
    fireEvent.click(
      screen.getByRole("button", { name: "Reboot all machines" }),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Reboot everything now" }),
    );
    const waiting = await screen.findByRole("button", { name: "Rebooting…" });
    expect(waiting).toHaveProperty("disabled", true);
    expect(server.sent("POST", "/api/system/reboot/all")).toHaveLength(1);
  });

  it("says why the machines were not rebooted, and lets you try again", async () => {
    const server = serve({
      "POST /api/system/reboot/all": reply(500, '{"error":"no quorum"}'),
    });
    render(<UpdatesPage />);
    fireEvent.click(
      screen.getByRole("button", { name: "Reboot all machines" }),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Reboot everything now" }),
    );
    expect((await screen.findByRole("alert")).textContent).toBe("no quorum");
    server.routes["POST /api/system/reboot/all"] = () =>
      Promise.reject(new Error("x"));
    fireEvent.click(
      screen.getByRole("button", { name: "Reboot all machines" }),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Reboot everything now" }),
    );
    expect(await screen.findByText("The machines did not answer")).toBeTruthy();
  });

  it("switches the channel and rebuilds from it", async () => {
    const server = serve({
      "GET /api/update/channel": { url: "github:me/fork", ref: "dev" },
      "PUT /api/update/channel": undefined,
      "POST /api/update": stream("[DONE]"),
    });
    render(<UpdatesPage />);
    const toggle = await screen.findByText("github:me/fork / dev");
    fireEvent.click(toggle);
    const url = screen.getByPlaceholderText(
      "github:DemyCode/yolab",
    ) as HTMLInputElement;
    const ref = screen.getByPlaceholderText("main") as HTMLInputElement;
    expect([url.value, ref.value]).toEqual(["github:me/fork", "dev"]);
    const go = screen.getByRole("button", { name: "Switch & rebuild" });
    fireEvent.change(ref, { target: { value: " " } });
    expect(go).toHaveProperty("disabled", true);
    fireEvent.change(url, { target: { value: " " } });
    expect(go).toHaveProperty("disabled", true);
    fireEvent.change(url, { target: { value: "github:me/other" } });
    fireEvent.change(ref, { target: { value: "v2" } });
    server.routes["GET /api/update/channel"] = {
      url: "github:me/other",
      ref: "v2",
    };
    fireEvent.click(go);
    expect(await screen.findByText("github:me/other / v2")).toBeTruthy();
    expect(server.sent("PUT", "/api/update/channel")[0].body).toEqual({
      url: "github:me/other",
      ref: "v2",
    });
    await waitFor(() =>
      expect(server.sent("POST", "/api/update")).toHaveLength(1),
    );
    expect(screen.queryByPlaceholderText("main")).toBeNull();
  });

  it("says why the channel was not switched, keeping what you typed", async () => {
    serve({
      "GET /api/update/channel": { url: "github:a/b", ref: "main" },
      "PUT /api/update/channel": reply(400, '{"error":"not a flake"}'),
    });
    render(<UpdatesPage />);
    fireEvent.click(await screen.findByText("github:a/b / main"));
    fireEvent.change(screen.getByPlaceholderText("main"), {
      target: { value: "x" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Switch & rebuild" }));
    expect(screen.getByRole("button", { name: "Switching…" })).toBeTruthy();
    expect((await screen.findByRole("alert")).textContent).toBe("not a flake");
    expect(
      (screen.getByPlaceholderText("main") as HTMLInputElement).value,
    ).toBe("x");
    fireEvent.click(screen.getByText("github:a/b / main"));
    expect(screen.queryByPlaceholderText("main")).toBeNull();
  });
});

const entry = (over: Record<string, unknown> = {}) => ({
  timestamp: "2026-01-01T10:00:00Z",
  unit: "k3s.service",
  priority: 6,
  message: "hello",
  ...over,
});

describe("LogsPage", () => {
  it("shows eight placeholder lines while loading", () => {
    fakeServer({ "GET /api/logs?limit=500": never() });
    const { container } = render(<LogsPage />);
    expect(container.querySelectorAll(".shimmer")).toHaveLength(8);
    expect(screen.getByRole("button", { name: "Refresh" }).innerHTML).toContain(
      "animate-spin",
    );
  });

  it("says when nothing matches, and why the logs could not be read", async () => {
    fakeServer({
      "GET /api/logs?limit=500": reply(500, '{"error":"journal gone"}'),
    });
    render(<LogsPage />);
    expect(await screen.findByText("Could not read the logs")).toBeTruthy();
    expect(screen.getByText("journal gone")).toBeTruthy();
    expect(screen.getByText("Nothing matches these filters.")).toBeTruthy();
  });

  it("lists lines coloured by priority, with short unit names and times", async () => {
    fakeServer({
      "GET /api/logs?limit=500": {
        entries: [
          entry({ priority: 3, message: "boom" }),
          entry({ priority: 4, message: "careful", timestamp: "" }),
          entry({ message: "fine", timestamp: "garbage", unit: "kernel" }),
        ],
        units: ["k3s.service", "kernel"],
        truncated: true,
      },
    });
    render(<LogsPage />);
    expect((await screen.findByText("boom")).className).toContain(
      "text-danger",
    );
    expect(screen.getByText("careful").className).toContain("text-warning");
    expect(screen.getByText("fine").className).toContain("text-fg-muted");
    expect(screen.getAllByText("k3s").length).toBeGreaterThan(1);
    expect(screen.getByText("Showing the most recent lines only")).toBeTruthy();
    const times = [...document.querySelectorAll("span.tabular-nums")].map(
      (s) => s.textContent,
    );
    expect(times[0]).not.toBe("");
    expect(times.slice(1)).toEqual(["", ""]);
  });

  it("narrows by level, time, unit and search, and refreshes", async () => {
    const lines = {
      entries: [entry()],
      units: ["k3s.service"],
      truncated: false,
    };
    const server = fakeServer({
      "GET /api/logs?limit=500": lines,
      "GET /api/logs?priority=4&limit=500": lines,
      "GET /api/logs?priority=4&since=1+hour+ago&limit=500": lines,
      "GET /api/logs?priority=4&since=1+hour+ago&unit=k3s.service&limit=500":
        lines,
      "GET /api/logs?priority=4&since=1+hour+ago&unit=k3s.service&search=disk&limit=500":
        lines,
    });
    render(<LogsPage />);
    await screen.findByText("hello");
    fireEvent.click(screen.getByRole("button", { name: "Warnings" }));
    expect(
      screen.getByRole("button", { name: "Warnings" }).className,
    ).toContain("font-medium");
    fireEvent.click(screen.getByRole("button", { name: "Last hour" }));
    await screen.findByRole("option", { name: "k3s" });
    fireEvent.change(screen.getByRole("combobox"), {
      target: { value: "k3s.service" },
    });
    fireEvent.change(screen.getByPlaceholderText("Search the messages"), {
      target: { value: " disk " },
    });
    await waitFor(() =>
      expect(server.calls.map((c) => c.url)).toContain(
        "/api/logs?priority=4&since=1+hour+ago&unit=k3s.service&search=disk&limit=500",
      ),
    );
    fireEvent.click(screen.getByRole("button", { name: "Clear search" }));
    expect(
      (screen.getByPlaceholderText("Search the messages") as HTMLInputElement)
        .value,
    ).toBe("");
    await settle();
    const before = server.calls.length;
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(server.calls.length).toBe(before + 1));
  });
});
