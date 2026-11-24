import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { AppDetailPage } from "./AppDetailPage";
import { fakeServer, never, reply, settle, stream } from "@/test/server";
import { renderAt } from "@/test/render";
import { appInfo, catalogApp } from "@/test/fixtures";
import type { AppInfo, PodInfo } from "@/types/apps";
import type { RestoreRecord } from "@/lib/apps";

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
}

const gitea = (over: Partial<AppInfo> = {}) =>
  appInfo({
    app_id: "gitea",
    instance_name: "gitea",
    chart_version: "1.0.0",
    ...over,
  });

function serve(app: AppInfo | null, over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/apps": app ? [app] : [],
    "GET /api/apps/catalog": [catalogApp({ tagline: "Git at home" })],
    "GET /api/backups/restores": [],
    [`GET /api/apps/${app?.instance_name ?? "gitea"}/outputs`]: never(),
    ...over,
  });
}

const open = (name = "gitea") =>
  renderAt(<AppDetailPage />, {
    path: `/app/${name}`,
    route: "/app/:instanceName",
  });

const where = () => screen.getByTestId("where").textContent;

describe("AppDetailPage, finding the service", () => {
  it("shows a placeholder while the services load", () => {
    fakeServer({
      "GET /api/apps": never(),
      "GET /api/apps/catalog": never(),
      "GET /api/backups/restores": never(),
    });
    const { container } = open();
    expect(container.querySelector(".shimmer")).not.toBeNull();
  });

  it("offers to try again when the services cannot be read", async () => {
    const server = serve(null, { "GET /api/apps": reply(502, "down") });
    open();
    fireEvent.click(await screen.findByRole("button", { name: "Try again" }));
    await waitFor(() =>
      expect(server.sent("GET", "/api/apps")).toHaveLength(2),
    );
  });

  it("says when there is no such service", async () => {
    serve(null);
    open("ghost");
    expect(
      await screen.findByText(/There is no service called “ghost”/),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "Back to my services" }));
    expect(where()).toBe("/");
  });
});

describe("AppDetailPage, a running service", () => {
  it("shows what it is, its version and what can be done with it", async () => {
    serve(gitea({ instance_name: "gitea-ab12", instance_id: "ab12" }));
    open("gitea-ab12");
    expect(await screen.findByRole("heading", { name: "Gitea" })).toBeTruthy();
    expect(screen.getByText("v1.0.0")).toBeTruthy();
    expect(screen.getByText("Git at home")).toBeTruthy();
    expect(screen.queryByText(/available$/)).toBeNull();
    expect(screen.queryByText("Name")).toBeNull();
    expect(screen.getByText("ab12")).toBeTruthy();
    expect(screen.getByText("Off")).toBeTruthy();
    fireEvent.click(screen.getByText("Its backups"));
    expect(where()).toBe("/system/backups/yolab-gitea-ab12");
  });

  it("opens its settings, a duplicate, or goes back home", async () => {
    serve(gitea());
    open();
    fireEvent.click(await screen.findByText("Change settings"));
    expect(where()).toBe("/add/gitea?edit=gitea");
  });

  it("opens a duplicate of it", async () => {
    serve(gitea());
    open();
    fireEvent.click(await screen.findByText("Duplicate"));
    expect(where()).toBe("/add/gitea?from=gitea");
  });

  it("links back to all services", async () => {
    serve(gitea());
    open();
    fireEvent.click(await screen.findByRole("link", { name: "My services" }));
    expect(where()).toBe("/");
  });

  it("offers the newer version in the catalog", async () => {
    serve(gitea(), {
      "GET /api/apps/catalog": [catalogApp({ chart_version: "2.0.0" })],
    });
    open();
    expect(await screen.findByText(/2\.0\.0 available/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Update to 2.0.0" }));
    expect(where()).toBe("/add/gitea?update=gitea");
  });

  it("says where it came from, what is inside it and its own name", async () => {
    serve(
      gitea({ app_id: "media", instance_name: "films", instance_id: "zz" }),
      {
        "GET /api/apps/catalog": [
          catalogApp({
            id: "media",
            name: "Media",
            repo: "friends",
            members: ["jellyfin", "sonarr"],
          }),
        ],
      },
    );
    open("films");
    expect(
      await screen.findByText('"friends", a source you added yourself'),
    ).toBeTruthy();
    expect(
      screen.getByText("jellyfin, sonarr, updated and removed together"),
    ).toBeTruthy();
    expect(screen.getByText("Name")).toBeTruthy();
  });

  it("says when its chart is no longer in the catalog", async () => {
    serve(gitea({ app_id: "gone", chart_version: "" }), {
      "GET /api/apps/catalog": reply(500, ""),
    });
    open();
    expect(
      await screen.findByText(
        "Installed from a chart that is no longer in the catalog.",
      ),
    ).toBeTruthy();
    expect(screen.getByText("unknown")).toBeTruthy();
  });

  it("takes the version from the catalog when the service does not say", async () => {
    serve(gitea({ chart_version: "" }));
    open();
    expect(await screen.findByText("v1.0.0")).toBeTruthy();
  });

  it.each([
    [
      { enabled: true, schedule: "0 3 * * *", last_ok_at: null, running: true },
      "Backing up now…",
    ],
    [
      {
        enabled: true,
        schedule: "0 3 * * *",
        last_ok_at: null,
        running: false,
      },
      /not saved yet/,
    ],
    [
      {
        enabled: true,
        schedule: "0 3 * * *",
        last_ok_at: new Date().toISOString(),
        running: false,
      },
      /last saved/,
    ],
  ])("describes its backups", async (backup, text) => {
    serve(gitea({ backup }));
    open();
    expect(await screen.findByText(text)).toBeTruthy();
  });
});

describe("AppDetailPage, banners", () => {
  it("is being removed, with little else to do", async () => {
    serve(
      gitea({
        status: "uninstalling",
        detail: "",
        since: new Date().toISOString(),
      }),
    );
    open();
    expect(await screen.findAllByText("Being removed")).toHaveLength(2);
    expect(screen.getByText(/Your request was saved/)).toBeTruthy();
    expect(screen.getByText("Started a few seconds ago.")).toBeTruthy();
    expect(screen.queryByText("What it is")).toBeNull();
    expect(screen.queryByText("Its backups")).toBeNull();
    expect(screen.queryByText("Remove Gitea")).toBeNull();
  });

  it("failed, with what the system said, and keeps it for you to look at", async () => {
    serve(
      gitea({
        status: "failed",
        detail: "Image not found",
        technical: "ErrImagePull",
      }),
    );
    open();
    expect(await screen.findByText("The installation failed")).toBeTruthy();
    expect(screen.getByText("Image not found")).toBeTruthy();
    expect(screen.getByText("ErrImagePull")).toBeTruthy();
    expect(
      screen.getByText("It is kept so you can see what went wrong."),
    ).toBeTruthy();
    expect(screen.queryByText("Remove Gitea")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Change settings" }));
    expect(where()).toBe("/add/gitea?edit=gitea");
  });

  it("failed, trying again on its own", async () => {
    serve(
      gitea({
        status: "failed",
        detail: "",
        retry_at: new Date(Date.now() + 120_000).toISOString(),
      }),
    );
    open();
    expect(
      await screen.findByText(
        /^It keeps trying on its own\. Next try in about/,
      ),
    ).toBeTruthy();
    expect(screen.getByText("It did not say why.")).toBeTruthy();
  });

  it("stopped working, and keeps trying", async () => {
    serve(
      gitea({
        status: "stopped",
        reason: "Out of memory",
        retry_at: new Date(Date.now() + 1000).toISOString(),
      }),
    );
    open();
    expect(await screen.findByText("It stopped working")).toBeTruthy();
    expect(screen.getByText("Out of memory")).toBeTruthy();
    expect(
      screen.getByText(
        "It keeps trying to start again on its own. Trying to start it again now…",
      ),
    ).toBeTruthy();
  });

  it("stopped working, without a next try", async () => {
    serve(gitea({ status: "stopped" }));
    open();
    expect(
      await screen.findByText("It keeps trying to start again on its own."),
    ).toBeTruthy();
  });

  it("is copying its files", async () => {
    serve(gitea({ status: "copying", detail: "" }));
    open();
    expect(
      await screen.findByText(
        "It starts on its own once its files are in place.",
      ),
    ).toBeTruthy();
  });

  it("says what it is copying", async () => {
    serve(gitea({ status: "copying", detail: "3 of 5 folders" }));
    open();
    expect(await screen.findByText("3 of 5 folders")).toBeTruthy();
  });

  it("is starting up", async () => {
    serve(gitea({ status: "starting", detail: "" }));
    open();
    expect(
      await screen.findByText(
        "This usually takes a minute or two the first time.",
      ),
    ).toBeTruthy();
  });
});

describe("AppDetailPage, updating", () => {
  const failed = () => gitea({ status: "failed" });

  it("retries a failed install and says the version it reached", async () => {
    const server = serve(failed(), {
      "POST /api/apps/gitea/update": stream("Now on version 1.2.0", "[DONE]"),
    });
    open();
    fireEvent.click(await screen.findByRole("button", { name: /Try again/ }));
    server.routes["GET /api/apps"] = [gitea()];
    expect(
      await screen.findByText("Gitea now runs version 1.2.0"),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    await waitFor(() =>
      expect(screen.queryByText("Gitea now runs version 1.2.0")).toBeNull(),
    );
    expect(server.sent("GET", "/api/apps/catalog")).toHaveLength(2);
  });

  it("says it was reinstalled when no version was named", async () => {
    const server = serve(failed(), {
      "POST /api/apps/gitea/update": stream("[DONE]"),
    });
    open();
    fireEvent.click(await screen.findByRole("button", { name: /Try again/ }));
    server.routes["GET /api/apps"] = [gitea()];
    expect(await screen.findByText("Gitea was reinstalled")).toBeTruthy();
  });

  it("says why the update did not finish", async () => {
    serve(failed(), {
      "POST /api/apps/gitea/update": stream("[ERROR] chart missing"),
    });
    open();
    fireEvent.click(await screen.findByRole("button", { name: /Try again/ }));
    await waitFor(() =>
      expect(screen.queryByText("The installation failed")).toBeTruthy(),
    );
    expect(
      await screen.findByText("The update did not finish: chart missing"),
    ).toBeTruthy();
  });

  it("shows the update error above a stopped service", async () => {
    serve(gitea({ status: "stopped" }), {
      "GET /api/apps/catalog": [catalogApp({ chart_version: "2.0.0" })],
    });
    open();
    await screen.findByText("It stopped working");
  });
});

const record = (over: Partial<RestoreRecord> = {}): RestoreRecord => ({
  id: "r1",
  namespace: "yolab-gitea",
  started_at: new Date().toISOString(),
  state: "running",
  ...over,
});

describe("AppDetailPage, restoring", () => {
  it("says it is being restored, and when its progress cannot be read", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const server = serve(gitea(), { "GET /api/backups/restores": [record()] });
    open();
    expect(await screen.findByText("Restoring Gitea")).toBeTruthy();
    expect(screen.queryByText("Change settings")).toBeNull();
    server.routes["GET /api/backups/restores"] = reply(
      502,
      '{"error":"no quorum"}',
    );
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(
      await screen.findByText(
        "Its progress could not be read just now: no quorum",
      ),
    ).toBeTruthy();
    server.routes["GET /api/backups/restores"] = () => Promise.reject("x");
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(
      await screen.findByText(
        "Its progress could not be read just now: The server did not answer.",
      ),
    ).toBeTruthy();
    server.routes["GET /api/backups/restores"] = [record()];
    await act(() => vi.advanceTimersByTimeAsync(5000));
    await waitFor(() =>
      expect(screen.queryByText(/Its progress could not be read/)).toBeNull(),
    );
  });

  it("says a restore failed, offers another backup, and can be dismissed", async () => {
    serve(gitea(), {
      "GET /api/backups/restores": [
        record({
          state: "failed",
          finished_at: new Date().toISOString(),
          error: "repo locked",
        }),
      ],
    });
    open();
    expect(await screen.findByText("The restore did not finish")).toBeTruthy();
    expect(screen.getByText("repo locked")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    await waitFor(() =>
      expect(screen.queryByText("The restore did not finish")).toBeNull(),
    );
  });

  it("names a restore failure that came without a reason", async () => {
    serve(gitea(), {
      "GET /api/backups/restores": [
        record({ state: "failed", finished_at: new Date().toISOString() }),
      ],
    });
    open();
    expect(
      await screen.findByText("Something went wrong while restoring it."),
    ).toBeTruthy();
    fireEvent.click(
      screen.getByRole("button", { name: "Pick a backup again" }),
    );
    expect(where()).toBe("/system/backups/yolab-gitea");
  });

  it("says it was restored, and can be dismissed", async () => {
    serve(gitea(), {
      "GET /api/backups/restores": [
        record({ state: "succeeded", finished_at: new Date().toISOString() }),
      ],
    });
    open();
    expect(await screen.findByText("Gitea is restored")).toBeTruthy();
    expect(screen.getByText(/finished/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    await waitFor(() =>
      expect(screen.queryByText("Gitea is restored")).toBeNull(),
    );
  });

  it("ignores restore answers that come after the page went away", async () => {
    const ok = deferred<unknown>();
    serve(gitea(), { "GET /api/backups/restores": () => ok.promise });
    const a = open();
    await screen.findByRole("heading", { name: "Gitea" });
    a.unmount();
    ok.resolve([record()]);
    const bad = deferred<unknown>();
    serve(gitea(), { "GET /api/backups/restores": () => bad.promise });
    const b = open();
    await screen.findByRole("heading", { name: "Gitea" });
    b.unmount();
    bad.reject(new Error("x"));
    await settle();
    expect(document.body.textContent).toBe("");
  });
});

describe("AppDetailPage, removing", () => {
  const data = { delete: ["gitea"], shared: [], kept: [] };

  it("removes it with its backups when asked, then goes home", async () => {
    const server = serve(gitea(), {
      "GET /api/apps/gitea/data": data,
      "DELETE /api/apps/gitea?delete_backups=true": undefined,
    });
    open();
    fireEvent.click(await screen.findByText("Remove Gitea"));
    expect(screen.getByText(/Its backups are kept/)).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Also delete its backups"));
    expect(screen.getByText(/Its backups are deleted too/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Remove it" }));
    await waitFor(() => expect(where()).toBe("/"));
    expect(
      server.sent("DELETE", "/api/apps/gitea?delete_backups=true"),
    ).toHaveLength(1);
  });

  it("says why it was not removed", async () => {
    const server = serve(gitea({ status: "failed" }), {
      "GET /api/apps/gitea/data": data,
      "DELETE /api/apps/gitea": reply(409, '{"error":"still copying"}'),
    });
    open();
    fireEvent.click(await screen.findByRole("button", { name: "Remove it" }));
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: "Remove it",
      }),
    );
    expect(
      await screen.findByText("It could not be removed: still copying"),
    ).toBeTruthy();
    server.routes["DELETE /api/apps/gitea"] = () => Promise.reject("x");
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: "Remove it",
      }),
    );
    expect(
      await screen.findByText(
        "It could not be removed: The server did not accept that.",
      ),
    ).toBeTruthy();
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: "Cancel",
      }),
    );
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

const pod = (over: Partial<PodInfo> = {}): PodInfo => ({
  name: "gitea-0",
  phase: "Running",
  ready: true,
  node: "node1",
  ...over,
});

describe("AppDetailPage, technical details", () => {
  const details = () =>
    fireEvent.click(screen.getByRole("button", { name: /Technical details/ }));

  it("lists the running parts by member, with their containers and events", async () => {
    serve(gitea({ config: { subdomain: "git", replicas: 2 } }), {
      "GET /api/apps/gitea/pods": [
        pod({
          restarts: 3,
          containers: [
            {
              name: "app",
              init: false,
              ready: true,
              state: "running",
              restarts: 0,
            },
            {
              name: "migrate",
              init: true,
              ready: false,
              state: "Completed",
              restarts: 2,
            },
          ],
          events: [
            {
              warning: true,
              reason: "BackOff",
              message: "",
              at: new Date().toISOString(),
              count: 4,
            },
            {
              warning: false,
              reason: "Pulled",
              message: "Pulled image",
              at: new Date().toISOString(),
              count: 1,
            },
          ],
        }),
        pod({
          name: "db-0",
          member: "postgres",
          ready: false,
          phase: "Pending",
          node: undefined,
          containers: [
            {
              name: "db",
              init: false,
              ready: false,
              state: "PodInitializing",
              restarts: 0,
            },
            {
              name: "side",
              init: false,
              ready: false,
              state: "running",
              restarts: 0,
            },
            {
              name: "boom",
              init: false,
              ready: false,
              state: "CrashLoopBackOff",
              restarts: 1,
            },
          ],
        }),
      ],
    });
    open();
    await screen.findByRole("heading", { name: "Gitea" });
    details();
    expect(await screen.findByText("gitea-0")).toBeTruthy();
    expect(screen.getByText("postgres")).toBeTruthy();
    expect(screen.getByText("On node1 · restarted 3×")).toBeTruthy();
    expect(screen.getByText("Not placed on a machine yet")).toBeTruthy();
    expect(screen.getByText("(setup)")).toBeTruthy();
    expect(screen.getByText("Completed · 2×")).toBeTruthy();
    expect(screen.getByText("boom: CrashLoopBackOff")).toBeTruthy();
    expect(screen.getByText(/BackOff \(4×\)/).className).toContain(
      "text-warning",
    );
    expect(screen.getByText(/Pulled image/).className).toContain(
      "text-fg-subtle",
    );
    const dots = [...document.querySelectorAll("li li span.h-1\\.5")].map(
      (d) => d.className,
    );
    expect(dots.map((c) => c.match(/bg-(success|warning|danger)/)![1])).toEqual(
      ["success", "success", "warning", "warning", "danger"],
    );
    expect(screen.getByText("git")).toBeTruthy();
    expect(screen.getByText("2")).toBeTruthy();
  });

  it("says when nothing runs, and shows no settings when it has none", async () => {
    serve(gitea(), { "GET /api/apps/gitea/pods": [] });
    open();
    await screen.findByRole("heading", { name: "Gitea" });
    details();
    expect(await screen.findByText("Nothing running right now.")).toBeTruthy();
    expect(screen.queryByText("Installed with")).toBeNull();
  });

  it("says why the running parts could not be read, keeping what it had", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const server = serve(gitea(), {
      "GET /api/apps/gitea/pods": reply(500, '{"error":"api down"}'),
    });
    open();
    await screen.findByRole("heading", { name: "Gitea" });
    details();
    expect(
      await screen.findByText("The running parts could not be read: api down"),
    ).toBeTruthy();
    server.routes["GET /api/apps/gitea/pods"] = [pod()];
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(await screen.findByText("gitea-0")).toBeTruthy();
    expect(screen.queryByText(/could not be read/)).toBeNull();
    server.routes["GET /api/apps/gitea/pods"] = () => Promise.reject("x");
    await act(() => vi.advanceTimersByTimeAsync(5000));
    expect(
      await screen.findByText(
        "The running parts could not be read: The server did not answer.",
      ),
    ).toBeTruthy();
    expect(screen.getByText("gitea-0")).toBeTruthy();
  });

  it("stops reading the running parts once closed", async () => {
    const ok = deferred<unknown>();
    const bad = deferred<unknown>();
    let n = 0;
    serve(gitea(), {
      "GET /api/apps/gitea/pods": () => (++n === 1 ? ok.promise : bad.promise),
    });
    open();
    await screen.findByRole("heading", { name: "Gitea" });
    details();
    details();
    ok.resolve([pod()]);
    details();
    details();
    bad.reject(new Error("x"));
    await settle();
    expect(screen.queryByText("gitea-0")).toBeNull();
  });

  it("follows a part's logs, stops them, and switches to another part", async () => {
    const first = deferred<Response>();
    serve(gitea(), {
      "GET /api/apps/gitea/pods": [pod(), pod({ name: "gitea-1" })],
      "GET /api/apps/gitea/logs/gitea-0": () => first.promise,
      "GET /api/apps/gitea/logs/gitea-1": stream("hello", "world"),
    });
    open();
    await screen.findByRole("heading", { name: "Gitea" });
    details();
    const logs = await screen.findAllByRole("button", { name: "Logs" });
    fireEvent.click(logs[0]);
    expect(
      screen.getByText(
        "Connected — waiting for this service to print something…",
      ),
    ).toBeTruthy();
    expect(screen.getByText("Live")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(screen.getByText("Stopped")).toBeTruthy();
    expect(screen.getByText("This service printed nothing.")).toBeTruthy();
    fireEvent.click(screen.getAllByRole("button", { name: "Logs" })[0]);
    fireEvent.click(screen.getByRole("button", { name: "Logs" }));
    expect(await screen.findByText(/hello\s+world/)).toBeTruthy();
    first.resolve(stream("late line", "[DONE]")());
    await settle();
    expect(screen.queryByText(/late line/)).toBeNull();
    await waitFor(() => expect(screen.getByText("Stopped")).toBeTruthy());
  });

  it("stops following logs when the page goes away", async () => {
    const server = serve(gitea(), {
      "GET /api/apps/gitea/pods": [pod()],
      "GET /api/apps/gitea/logs/gitea-0": never(),
    });
    const fetchMock = globalThis.fetch as unknown as ReturnType<typeof vi.fn>;
    const view = open();
    await screen.findByRole("heading", { name: "Gitea" });
    details();
    fireEvent.click(await screen.findByRole("button", { name: "Logs" }));
    const signal = (fetchMock.mock.calls.at(-1)![1] as RequestInit).signal!;
    view.unmount();
    expect(signal.aborted).toBe(true);
    expect(server.sent("GET", "/api/apps/gitea/logs/gitea-0")).toHaveLength(1);
  });
});

describe("AppDetailPage, removing a running service", () => {
  it("says above it why it was not removed", async () => {
    serve(gitea(), {
      "GET /api/apps/gitea/data": { delete: [], shared: [], kept: [] },
      "DELETE /api/apps/gitea": reply(409, '{"error":"busy"}'),
    });
    open();
    fireEvent.click(await screen.findByText("Remove Gitea"));
    fireEvent.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: "Remove it",
      }),
    );
    expect(await screen.findByText("It could not be removed")).toBeTruthy();
    expect(screen.getByText("busy")).toBeTruthy();
  });
});
