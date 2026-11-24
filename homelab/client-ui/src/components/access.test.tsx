import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AppAccess } from "./AppAccess";
import { AppSources } from "./AppSources";
import { fakeServer, never, reply, settle } from "@/test/server";
import { renderAt } from "@/test/render";
import type { AppOutput } from "@/types/apps";

const out = (over: Partial<AppOutput>): AppOutput => ({
  key: "k",
  title: "T",
  format: "text",
  value: "v",
  found_at: new Date().toISOString(),
  ...over,
});

const link = { label: "Web", url: "https://git.example" };

describe("AppAccess", () => {
  it("shows nothing for an app with no links and no outputs", async () => {
    fakeServer({ "GET /api/apps/a/outputs": { outputs: [] } });
    const { container } = render(<AppAccess instanceName="a" links={[]} />);
    expect(screen.getByLabelText("Loading")).toBeTruthy();
    await waitFor(() => expect(container.innerHTML).toBe(""));
  });

  it("lists links with open and copy, and waits on outputs not yet reported", async () => {
    fakeServer({
      "GET /api/apps/a/outputs": {
        outputs: [
          out({ key: "url", format: "uri", value: "https://hidden" }),
          out({
            key: "pending",
            title: "Admin code",
            value: null,
            found_at: null,
          }),
          out({ key: "half", title: "Half", found_at: null }),
        ],
      },
    });
    render(<AppAccess instanceName="a" links={[link]} />);
    expect(screen.getByTitle("https://git.example")).toBeTruthy();
    expect(
      screen.getByRole("link", { name: "Open Web" }).getAttribute("href"),
    ).toBe(link.url);
    expect(screen.getByRole("button", { name: "Copy Web" })).toBeTruthy();
    expect(await screen.findByText("Admin code")).toBeTruthy();
    expect(screen.getByText("Half")).toBeTruthy();
    expect(screen.queryByText("https://hidden")).toBeNull();
    expect(screen.getAllByLabelText("Loading")).toHaveLength(2);
  });

  it("shows text, hides secrets until asked, and boxes long values", async () => {
    fakeServer({
      "GET /api/apps/a/outputs": {
        outputs: [
          out({ key: "user", title: "User", value: "admin" }),
          out({
            key: "pw",
            title: "Password",
            format: "secret",
            value: "s3cret",
          }),
          out({
            key: "cfg",
            title: "Config",
            format: "multiline",
            value: "a\nb",
          }),
        ],
      },
    });
    const { container } = render(<AppAccess instanceName="a" links={[]} />);
    expect(await screen.findByTitle("admin")).toBeTruthy();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(screen.getByText("••••••••••••")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Show Password" }));
    expect(screen.getByTitle("s3cret")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Hide Password" }));
    expect(screen.queryByTitle("s3cret")).toBeNull();
    expect(container.querySelector("pre")!.textContent).toBe("a\nb");
    expect(screen.getAllByText(/^Reported /)).toHaveLength(3);
  });

  it("checks again on request", async () => {
    const server = fakeServer({
      "GET /api/apps/a/outputs": { outputs: [] },
      "POST /api/apps/a/scan-outputs": { outputs: [out({ title: "Found" })] },
    });
    render(<AppAccess instanceName="a" links={[link]} />);
    fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    expect(screen.getByRole("button", { name: "Checking…" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(await screen.findByText("Found")).toBeTruthy();
    expect(server.sent("POST", "/api/apps/a/scan-outputs")).toHaveLength(1);
  });

  it("says the details could not be loaded, until a check works", async () => {
    const server = fakeServer({
      "GET /api/apps/a/outputs": reply(500, "down"),
      "POST /api/apps/a/scan-outputs": reply(500, "down"),
    });
    render(<AppAccess instanceName="a" links={[]} />);
    expect(
      await screen.findByText(/could not be loaded right now/),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    await waitFor(() =>
      expect(server.sent("POST", "/api/apps/a/scan-outputs")).toHaveLength(1),
    );
    await screen.findByRole("button", { name: "Check again" });
    expect(screen.getByText(/could not be loaded right now/)).toBeTruthy();
    server.routes["POST /api/apps/a/scan-outputs"] = {
      outputs: [out({ title: "Back" })],
    };
    fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    expect(await screen.findByText("Back")).toBeTruthy();
    expect(screen.queryByText(/could not be loaded/)).toBeNull();
  });

  it("asks again every fifteen seconds and stops when it goes away", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const server = fakeServer({ "GET /api/apps/a/outputs": { outputs: [] } });
    const { unmount } = render(<AppAccess instanceName="a" links={[link]} />);
    await waitFor(() => expect(server.calls).toHaveLength(1));
    await act(async () => vi.advanceTimersByTime(15_000));
    await waitFor(() => expect(server.calls).toHaveLength(2));
    server.routes["GET /api/apps/a/outputs"] = reply(500, "down");
    await act(async () => vi.advanceTimersByTime(15_000));
    await waitFor(() => expect(server.calls).toHaveLength(3));
    await settle();
    expect(screen.queryByText(/could not be loaded/)).toBeNull();
    unmount();
    await act(async () => vi.advanceTimersByTime(15_000));
    expect(server.calls).toHaveLength(3);
  });

  it("drops answers for an app it has left", async () => {
    let answer!: (r: Response) => void;
    let fail!: (e: Error) => void;
    let n = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((resolve, reject) => {
            if (n === 0) answer = resolve;
            if (n === 1) fail = reject;
            n += 1;
          }),
      ),
    );
    const { rerender } = render(<AppAccess instanceName="a" links={[link]} />);
    rerender(<AppAccess instanceName="b" links={[link]} />);
    rerender(<AppAccess instanceName="c" links={[link]} />);
    await act(async () => {
      answer(
        new Response(JSON.stringify({ outputs: [out({ title: "Stale" })] })),
      );
      fail(new Error("x"));
    });
    expect(screen.queryByText("Stale")).toBeNull();
    expect(screen.queryByText(/could not be loaded/)).toBeNull();
  });
});

const builtIn = {
  name: "yolab",
  url: "https://charts.yolab.io",
  removable: false,
};
const extra = { name: "community", url: "https://c.example", removable: true };

describe("AppSources", () => {
  it("lists the sources, marking the built-in one", async () => {
    fakeServer({ "GET /api/apps/repos": [builtIn, extra] });
    renderAt(<AppSources />);
    expect(screen.getByText("Loading…")).toBeTruthy();
    expect(await screen.findByText("Built in")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Remove yolab" })).toBeNull();
    expect(
      screen.getByRole("button", { name: "Remove community" }),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("link"));
    expect(screen.getByTestId("where").textContent).toBe("/add/custom");
  });

  it("adds a source after a warning and tells the page", async () => {
    const onChanged = vi.fn();
    const server = fakeServer({
      "GET /api/apps/repos": [builtIn],
      "POST /api/apps/repos": undefined,
    });
    renderAt(<AppSources onChanged={onChanged} />);
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    expect(screen.getByText(/Only add a source you would trust/)).toBeTruthy();
    const save = screen.getByRole("button", { name: "Add source" });
    expect(save).toHaveProperty("disabled", true);
    fireEvent.change(screen.getByLabelText("Source name"), {
      target: { value: " community " },
    });
    expect(save).toHaveProperty("disabled", true);
    fireEvent.change(screen.getByLabelText("Source URL"), {
      target: { value: " https://c.example " },
    });
    server.routes["GET /api/apps/repos"] = [builtIn, extra];
    fireEvent.click(save);
    expect(await screen.findByText("community")).toBeTruthy();
    expect(server.sent("POST", "/api/apps/repos")[0].body).toEqual({
      name: "community",
      url: "https://c.example",
    });
    expect(onChanged).toHaveBeenCalledOnce();
    expect(screen.queryByLabelText("Source name")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    expect(
      (screen.getByLabelText("Source name") as HTMLInputElement).value,
    ).toBe("");
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    expect(screen.queryByLabelText("Source name")).toBeNull();
  });

  it("says why a source was not added", async () => {
    const server = fakeServer({
      "GET /api/apps/repos": [builtIn],
      "POST /api/apps/repos": reply(400, '{"error":"not a chart repo"}'),
    });
    renderAt(<AppSources />);
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    fireEvent.change(screen.getByLabelText("Source name"), {
      target: { value: "x" },
    });
    fireEvent.change(screen.getByLabelText("Source URL"), {
      target: { value: "y" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add source" }));
    expect(await screen.findByText("not a chart repo")).toBeTruthy();
    server.routes["POST /api/apps/repos"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Add source" }));
    expect(await screen.findByText("Could not add that source")).toBeTruthy();
  });

  it("removes a source, and says why when it cannot", async () => {
    const onChanged = vi.fn();
    const server = fakeServer({
      "GET /api/apps/repos": [builtIn, extra, { ...extra, name: "a b" }],
      "DELETE /api/apps/repos/community": undefined,
      "DELETE /api/apps/repos/a%20b": reply(409, '{"error":"in use"}'),
    });
    renderAt(<AppSources onChanged={onChanged} />);
    fireEvent.click(await screen.findByRole("button", { name: "Remove a b" }));
    expect(await screen.findByText("in use")).toBeTruthy();
    server.routes["GET /api/apps/repos"] = [builtIn, { ...extra, name: "a b" }];
    fireEvent.click(screen.getByRole("button", { name: "Remove community" }));
    await waitFor(() => expect(screen.queryByText("community")).toBeNull());
    expect(onChanged).toHaveBeenCalledOnce();
    server.routes["DELETE /api/apps/repos/a%20b"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Remove a b" }));
    expect(
      await screen.findByText("Could not remove that source"),
    ).toBeTruthy();
  });

  it("refreshes the catalogs and counts what is available", async () => {
    const onChanged = vi.fn();
    const server = fakeServer({
      "GET /api/apps/repos": [builtIn],
      "POST /api/apps/repos/sync": { yolab: { ok: true, charts: 1 } },
    });
    renderAt(<AppSources onChanged={onChanged} />);
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByText("1 service available")).toBeTruthy();
    expect(onChanged).toHaveBeenCalledOnce();
    server.routes["POST /api/apps/repos/sync"] = {
      yolab: { ok: true, charts: 40 },
      other: { ok: true },
    };
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByText("40 services available")).toBeTruthy();
  });

  it("names the sources that could not be read, and hides the count then", async () => {
    const server = fakeServer({
      "GET /api/apps/repos": [builtIn],
      "POST /api/apps/repos/sync": {
        yolab: { ok: true, charts: 3 },
        a: { ok: false, error: "404" },
        b: { ok: false },
      },
    });
    renderAt(<AppSources />);
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(
      await screen.findByText("a: 404 · b: could not be read"),
    ).toBeTruthy();
    expect(screen.queryByText(/available/)).toBeNull();
    server.routes["POST /api/apps/repos/sync"] = { a: { ok: false } };
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByText("a: could not be read")).toBeTruthy();
    server.routes["POST /api/apps/repos/sync"] = reply(
      500,
      '{"error":"offline"}',
    );
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByText("offline")).toBeTruthy();
    server.routes["POST /api/apps/repos/sync"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByText("Could not refresh")).toBeTruthy();
  });

  it("works without anyone to tell", async () => {
    fakeServer({
      "GET /api/apps/repos": never(),
      "POST /api/apps/repos/sync": {},
      "POST /api/apps/repos": undefined,
      "DELETE /api/apps/repos/community": undefined,
    });
    renderAt(<AppSources />);
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await settle();
    expect(screen.queryByText(/available/)).toBeNull();
  });
});
