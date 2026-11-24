import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { BackupTimelinePage } from "./BackupTimelinePage";
import { fakeServer, never, reply, settle } from "@/test/server";
import { renderAt } from "@/test/render";
import type { ProtectedApp, RestorePoint, RestorePoints } from "@/lib/backups";

vi.mock("@/components/BackupTimeline", () => ({
  BackupTimeline: ({
    points,
    selected,
    onSelect,
  }: {
    points: RestorePoint[];
    selected: string | null;
    onSelect: (id: string) => void;
  }) => (
    <div data-testid="timeline" data-selected={selected ?? ""}>
      {points.map((p) => (
        <button key={p.snapshot_id} onClick={() => onSelect(p.snapshot_id)}>
          pick {p.snapshot_id}
        </button>
      ))}
    </div>
  ),
}));

const NS = "yolab-gitea";
const base = `/api/backups/apps/${NS}`;

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
}

const app = (over: Partial<ProtectedApp> = {}): ProtectedApp => ({
  namespace: NS,
  instance_name: "gitea",
  app_id: "gitea",
  enabled: true,
  schedule: "0 3 * * *",
  state: "ok",
  last_ok_at: new Date().toISOString(),
  error: null,
  ...over,
});

const point = (
  id: string,
  daysAgo: number,
  over: Partial<RestorePoint> = {},
): RestorePoint => ({
  snapshot_id: id,
  time: new Date(Date.now() - daysAgo * 86_400_000).toISOString(),
  ...over,
});

const copies = (
  points: RestorePoint[],
  over: Partial<RestorePoints> = {},
): RestorePoints => ({
  configured: true,
  points,
  ...over,
});

function serve(over: Record<string, unknown> = {}) {
  return fakeServer({
    [`GET ${base}/points`]: copies([point("s1", 1), point("s2", 3)]),
    "GET /api/backups/protected": { configured: true, apps: [app()] },
    ...over,
  });
}

const open = () =>
  renderAt(<BackupTimelinePage />, {
    path: `/system/backups/${NS}`,
    route: "/system/backups/:namespace",
  });

const where = () => screen.getByTestId("where").textContent;
const header = () => document.querySelector("header p")!.textContent;

describe("BackupTimelinePage, loading and listing", () => {
  it("looks for its backups, named after its namespace until it is known", () => {
    serve({
      [`GET ${base}/points`]: never(),
      "GET /api/backups/protected": never(),
    });
    open();
    expect(screen.getByRole("heading").textContent).toBe("gitea");
    expect(screen.getByText("Looking for its backups…")).toBeTruthy();
    expect(header()).toBe(
      "Each copy is a moment you can bring this service back to.",
    );
    fireEvent.click(screen.getByRole("link", { name: "Backups" }));
    expect(where()).toBe("/system/backups");
  });

  it("says why its backups could not be listed", async () => {
    serve({ [`GET ${base}/points`]: reply(500, '{"error":"repo locked"}') });
    open();
    expect(await screen.findByText("repo locked")).toBeTruthy();
  });

  it("names the failure when the server did not answer", async () => {
    serve({ [`GET ${base}/points`]: () => Promise.reject("x") });
    open();
    expect(
      await screen.findByText("Its backups could not be listed."),
    ).toBeTruthy();
  });

  it("ignores answers that come after the page went away", async () => {
    const ok = deferred<unknown>();
    serve({ [`GET ${base}/points`]: () => ok.promise });
    open().unmount();
    ok.resolve(copies([point("late", 1)]));
    const bad = deferred<unknown>();
    serve({ [`GET ${base}/points`]: () => bad.promise });
    open().unmount();
    bad.reject(new Error("x"));
    await settle();
    expect(document.body.textContent).toBe("");
  });

  it("lists copies newest first, with what each one added", async () => {
    serve({
      [`GET ${base}/points`]: copies([
        point("old", 10),
        point("new", 0.5, { newest: true, added_bytes: 2048 }),
      ]),
    });
    open();
    const rows = await screen.findAllByRole("button", { pressed: false });
    expect(
      rows.map((r) => r.closest("[class]")!.textContent).join(""),
    ).toBeTruthy();
    expect(
      screen.getByText(/Newest · 2(\.\d)? KB not in the copy before/),
    ).toBeTruthy();
    expect(screen.getByTestId("timeline").dataset.selected).toBe("new");
    fireEvent.click(screen.getByRole("button", { name: "pick old" }));
    expect(screen.getByTestId("timeline").dataset.selected).toBe("old");
    expect(screen.getByText("Delete this copy")).toBeTruthy();
  });
});

describe("BackupTimelinePage, a removed service", () => {
  const removed = {
    "GET /api/backups/protected": { configured: true, apps: [] },
  };

  it("is marked removed, and deleting all its backups goes back to the list", async () => {
    const server = serve({
      ...removed,
      [`GET ${base}/points`]: copies([point("s1", 1), point("s2", 2)], {
        size_bytes: 1000,
        budget_bytes: 10e9,
      }),
      [`DELETE ${base}`]: undefined,
    });
    open();
    await screen.findByText(/no longer installed/);
    expect(header()).toBe("2 copies · 1 KB of 10 GB · Removed");
    expect(screen.queryByText("Backing up")).toBeNull();
    fireEvent.click(screen.getByText("Delete all its backups"));
    expect(screen.getByText(/freeing about 1 KB/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Delete them all" }));
    await waitFor(() => expect(where()).toBe("/system/backups"));
    expect(server.sent("DELETE", base)).toHaveLength(1);
  });

  it("says why its backups were not deleted, outside the settings", async () => {
    serve({ ...removed, [`DELETE ${base}`]: () => Promise.reject("x") });
    open();
    fireEvent.click(await screen.findByText("Delete all its backups"));
    expect(screen.queryByText(/freeing about/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Delete them all" }));
    expect(
      (await screen.findByText("Its backups were not deleted.")).className,
    ).toContain("text-danger");
  });

  it("says when every copy is gone, with nothing left to delete", async () => {
    serve({ ...removed, [`GET ${base}/points`]: copies([]) });
    open();
    expect(
      await screen.findByText("Every copy of this service has been deleted."),
    ).toBeTruthy();
    expect(screen.queryByText("Delete all its backups")).toBeNull();
  });
});

describe("BackupTimelinePage, backing up", () => {
  it("shows the last backup and starts one now", async () => {
    const server = serve({
      [`GET ${base}/points`]: copies([point("s1", 1)]),
      [`POST ${base}/run-now`]: undefined,
    });
    open();
    const section = within(
      (await screen.findByText("Backing up")).closest("section")!,
    );
    expect(screen.queryByTestId("timeline")).toBeNull();
    expect(header()).toBe("1 copy");
    fireEvent.click(section.getByRole("button", { name: "Back up now" }));
    expect(section.getByText("Backing up now…")).toBeTruthy();
    await waitFor(() =>
      expect(server.sent("GET", "/api/backups/protected").length).toBe(2),
    );
    expect(
      await section.findByRole("button", { name: "Back up now" }),
    ).toBeTruthy();
  });

  it("says why a backup did not start", async () => {
    const server = serve({
      [`POST ${base}/run-now`]: reply(409, '{"error":"busy"}'),
    });
    open();
    fireEvent.click(await screen.findByRole("button", { name: "Back up now" }));
    expect(await screen.findByText("busy")).toBeTruthy();
    server.routes[`POST ${base}/run-now`] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Back up now" }));
    expect(await screen.findByText("Could not start a backup.")).toBeTruthy();
  });

  it("is busy while a backup runs or waits, and says when it never ran", async () => {
    serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [app({ state: "queued", last_ok_at: null })],
      },
    });
    open();
    expect(
      await screen.findByRole("button", { name: "Backing up…" }),
    ).toHaveProperty("disabled", true);
    expect(screen.getByText("Backing up now…")).toBeTruthy();
  });

  it("says a service was never backed up", async () => {
    serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [app({ enabled: false, last_ok_at: null })],
      },
    });
    open();
    expect(await screen.findByText("Not backed up yet")).toBeTruthy();
    expect(header()).toContain("Backups off");
    expect(screen.queryByLabelText("How often")).toBeNull();
    expect(
      screen.getByText(/Its backups are off, so no new copy is made/),
    ).toBeTruthy();
  });

  it("turns automatic backups off, and changes how often", async () => {
    const server = serve({ "PUT /api/apps/gitea/backup": undefined });
    open();
    fireEvent.click(await screen.findByRole("switch"));
    expect(await screen.findByText("Saved")).toBeTruthy();
    expect(server.sent("PUT", "/api/apps/gitea/backup")[0].body).toEqual({
      enabled: false,
      schedule: "0 3 * * *",
    });
    await waitFor(() =>
      expect(screen.getByRole("switch")).toHaveProperty("disabled", false),
    );
    fireEvent.change(screen.getByLabelText("How often"), {
      target: { value: "0 */6 * * *" },
    });
    await waitFor(() =>
      expect(server.sent("PUT", "/api/apps/gitea/backup")).toHaveLength(2),
    );
    expect(server.sent("PUT", "/api/apps/gitea/backup")[1].body).toEqual({
      enabled: true,
      schedule: "0 */6 * * *",
    });
  });

  it("says why a setting was not saved", async () => {
    const server = serve({
      "PUT /api/apps/gitea/backup": reply(400, '{"error":"bad cron"}'),
    });
    open();
    fireEvent.click(await screen.findByRole("switch"));
    expect(await screen.findByText("bad cron")).toBeTruthy();
    server.routes["PUT /api/apps/gitea/backup"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("switch"));
    expect(await screen.findByText("That was not saved.")).toBeTruthy();
  });

  it("saves the space for its backups, starting from the default", async () => {
    let budget: number | null = null;
    const server = serve({
      [`GET ${base}/points`]: () =>
        copies([point("s1", 1)], { budget_bytes: budget }),
      [`PUT ${base}/budget`]: ({ body }: { body: unknown }) => {
        budget = (body as { budget_bytes: number }).budget_bytes;
        return undefined;
      },
    });
    open();
    const input = (await screen.findByLabelText(
      "Space in GB",
    )) as HTMLInputElement;
    expect(input.value).toBe("10");
    fireEvent.change(input, { target: { value: "25" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("Saved")).toBeTruthy();
    expect(server.sent("PUT", `${base}/budget`)[0].body).toEqual({
      budget_bytes: 25e9,
    });
    expect(
      ((await screen.findByLabelText("Space in GB")) as HTMLInputElement).value,
    ).toBe("25");
  });

  it("says why the space was not saved", async () => {
    serve({ [`PUT ${base}/budget`]: () => Promise.reject("x") });
    open();
    fireEvent.change(await screen.findByLabelText("Space in GB"), {
      target: { value: "5" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("The space was not saved.")).toBeTruthy();
  });
});

describe("BackupTimelinePage, banners", () => {
  it("offers to retry a failed backup", async () => {
    const server = serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [app({ state: "failed", error: "disk full" })],
      },
      [`POST ${base}/run-now`]: undefined,
    });
    open();
    expect(await screen.findByText("The last backup failed")).toBeTruthy();
    expect(screen.getByText("disk full")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() =>
      expect(server.sent("POST", `${base}/run-now`)).toHaveLength(1),
    );
  });

  it("offers more space, or turning backups off, when the newest copy does not fit", async () => {
    const server = serve({
      [`GET ${base}/points`]: copies([point("s1", 1)], {
        over_budget: true,
        size_bytes: 1.5e9,
        budget_bytes: 1e9,
      }),
      [`PUT ${base}/budget`]: undefined,
      "PUT /api/apps/gitea/backup": undefined,
    });
    open();
    expect(
      await screen.findByText(
        "Its newest copy alone needs more than its space",
      ),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Give it 2 GB" }));
    await waitFor(() =>
      expect(server.sent("PUT", `${base}/budget`)).toHaveLength(1),
    );
    expect(server.sent("PUT", `${base}/budget`)[0].body).toEqual({
      budget_bytes: 2e9,
    });
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Turn off its backups" }),
      ).toHaveProperty("disabled", false),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Turn off its backups" }),
    );
    await waitFor(() =>
      expect(server.sent("PUT", "/api/apps/gitea/backup")).toHaveLength(1),
    );
    expect(server.sent("PUT", "/api/apps/gitea/backup")[0].body).toEqual({
      enabled: false,
      schedule: "0 3 * * *",
    });
  });

  it("does not offer to turn off backups that are already off", async () => {
    serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [app({ enabled: false })],
      },
      [`GET ${base}/points`]: copies([point("s1", 1)], {
        over_budget: true,
        size_bytes: 1.5e9,
        budget_bytes: 1e9,
      }),
    });
    open();
    await screen.findByText("Its newest copy alone needs more than its space");
    expect(
      screen.queryByRole("button", { name: "Turn off its backups" }),
    ).toBeNull();
  });

  it("warns when there is no recent backup", async () => {
    const server = serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [app({ last_ok_at: "2020-01-01T00:00:00Z" })],
      },
      [`POST ${base}/run-now`]: undefined,
    });
    open();
    const banner = (await screen.findByText("No recent backup")).closest(
      "[role]",
    )!;
    fireEvent.click(
      within(banner as HTMLElement).getByRole("button", {
        name: "Back up now",
      }),
    );
    await waitFor(() =>
      expect(server.sent("POST", `${base}/run-now`)).toHaveLength(1),
    );
  });
});

describe("BackupTimelinePage, a single copy", () => {
  async function chooseFirst() {
    fireEvent.click(await screen.findByRole("button", { pressed: true }));
    fireEvent.click(screen.getByText("Delete this copy"));
  }

  it("says how much deleting a copy frees, then deletes it in the background", async () => {
    const gone = deferred<Response>();
    let listed = [point("s1", 1), point("s2", 3)];
    const server = serve({
      [`GET ${base}/points`]: () => copies(listed),
      [`GET ${base}/points/s1/frees`]: { frees_bytes: 3000 },
      [`DELETE ${base}/points/s1`]: () => gone.promise,
    });
    open();
    await chooseFirst();
    expect(await screen.findByText("It frees about 3 KB.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Delete it" }));
    expect(screen.getByText("Deleting 1 copy in the background…")).toBeTruthy();
    listed = [point("s2", 3)];
    gone.resolve(new Response(null, { status: 204 }));
    await waitFor(() =>
      expect(
        screen.queryByText("Deleting 1 copy in the background…"),
      ).toBeNull(),
    );
    expect(header()).toBe("1 copy");
    expect(server.sent("DELETE", `${base}/points/s1`)).toHaveLength(1);
  });

  it("tells what the space freed is not yet known, shared, or could not be worked out", async () => {
    const server = serve({ [`GET ${base}/points/s1/frees`]: never() });
    open();
    await chooseFirst();
    expect(
      screen.getByText("Working out how much space it frees…"),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    server.routes[`GET ${base}/points/s1/frees`] = { frees_bytes: null };
    fireEvent.click(screen.getByText("Delete this copy"));
    expect(
      await screen.findByText("The space it frees is known once it is gone."),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    server.routes[`GET ${base}/points/s1/frees`] = { frees_bytes: 0 };
    fireEvent.click(screen.getByText("Delete this copy"));
    expect(await screen.findByText(/It frees almost nothing/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    server.routes[`GET ${base}/points/s1/frees`] = reply(500);
    fireEvent.click(screen.getByText("Delete this copy"));
    expect(
      await screen.findByText(
        "How much space it frees could not be worked out.",
      ),
    ).toBeTruthy();
  });

  it("ignores how much a copy frees once the question was closed", async () => {
    const ok = deferred<unknown>();
    const bad = deferred<unknown>();
    let n = 0;
    serve({
      [`GET ${base}/points/s1/frees`]: () =>
        ++n === 1 ? ok.promise : bad.promise,
    });
    open();
    await chooseFirst();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    ok.resolve({ frees_bytes: 1 });
    fireEvent.click(screen.getByText("Delete this copy"));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    bad.reject(new Error("x"));
    await settle();
    expect(screen.queryByText(/frees/)).toBeNull();
  });

  it("says why a copy was not deleted, and keeps it", async () => {
    serve({
      [`GET ${base}/points/s1/frees`]: { frees_bytes: 1 },
      [`DELETE ${base}/points/s1`]: () => Promise.reject("x"),
    });
    open();
    await chooseFirst();
    fireEvent.click(screen.getByRole("button", { name: "Delete it" }));
    expect(
      await screen.findByText(/^The copy from .* was not deleted\.$/),
    ).toBeTruthy();
    expect(header()).toBe("2 copies");
  });

  it("opens a copy in the install page to restore it", async () => {
    serve({
      [`GET ${base}/definition?snapshot_id=s1`]: { app_id: "gitea" },
    });
    open();
    const restores = await screen.findAllByRole("button", { name: "Restore" });
    fireEvent.click(restores[0]);
    await waitFor(() =>
      expect(where()).toBe(`/add/gitea?restore=${NS}&snapshot=s1`),
    );
  });

  it("says why a copy could not be opened", async () => {
    const server = serve({
      [`GET ${base}/definition?snapshot_id=s1`]: { app_id: "" },
    });
    open();
    const restore = (
      await screen.findAllByRole("button", { name: "Restore" })
    )[0];
    fireEvent.click(restore);
    expect(screen.getByRole("button", { name: "Opening…" })).toBeTruthy();
    expect(
      await screen.findByText("That backup does not say which service it is."),
    ).toBeTruthy();
    server.routes[`GET ${base}/definition?snapshot_id=s1`] = () =>
      Promise.reject("x");
    fireEvent.click(screen.getAllByRole("button", { name: "Restore" })[0]);
    expect(
      await screen.findByText("That copy could not be opened."),
    ).toBeTruthy();
  });

  it("an error opening a copy that is not chosen still shows", async () => {
    serve({ [`GET ${base}/definition?snapshot_id=s2`]: { app_id: "" } });
    open();
    fireEvent.click(
      (await screen.findAllByRole("button", { name: "Restore" }))[1],
    );
    expect(
      await screen.findByText("That backup does not say which service it is."),
    ).toBeTruthy();
    expect(screen.getAllByText("Delete this copy")).toHaveLength(1);
  });
});

describe("BackupTimelinePage, deleting everything of an installed service", () => {
  it("deletes every copy and stays on the page", async () => {
    let listed = [point("s1", 1)];
    const server = serve({
      [`GET ${base}/points`]: () => copies(listed),
      [`DELETE ${base}`]: () => {
        listed = [];
        return undefined;
      },
    });
    open();
    expect(await screen.findByText(/turn its backups off first/)).toBeTruthy();
    fireEvent.click(screen.getByText("Delete all its backups"));
    fireEvent.click(screen.getByRole("button", { name: "Delete them all" }));
    expect(
      await screen.findByText("Back up now to make the first one."),
    ).toBeTruthy();
    expect(where()).toBe(`/system/backups/${NS}`);
    expect(server.sent("DELETE", base)).toHaveLength(1);
  });

  it("shows the background deletion while it runs, then why it failed", async () => {
    const gone = deferred<Response>();
    serve({ [`DELETE ${base}`]: () => gone.promise });
    open();
    fireEvent.click(await screen.findByText("Delete all its backups"));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    fireEvent.click(screen.getByText("Delete all its backups"));
    fireEvent.click(screen.getByRole("button", { name: "Delete them all" }));
    expect(
      screen.getByText("Deleting all its copies in the background…"),
    ).toBeTruthy();
    expect(screen.queryByText("Copies")).toBeNull();
    gone.resolve(new Response('{"error":"locked"}', { status: 423 }));
    expect(await screen.findByText("locked")).toBeTruthy();
    expect(screen.getByText("Copies")).toBeTruthy();
  });
});

describe("BackupTimelinePage, a failed reload", () => {
  it("keeps the copies it had and says the list is out of date", async () => {
    let n = 0;
    serve({
      [`GET ${base}/points`]: () =>
        ++n === 1
          ? copies([point("s1", 1), point("s2", 3)])
          : reply(500, '{"error":"repo busy"}')(),
      [`GET ${base}/points/s1/frees`]: { frees_bytes: 1 },
      [`DELETE ${base}/points/s1`]: undefined,
    });
    open();
    fireEvent.click(await screen.findByRole("button", { pressed: true }));
    fireEvent.click(screen.getByText("Delete this copy"));
    fireEvent.click(screen.getByRole("button", { name: "Delete it" }));
    expect((await screen.findByRole("alert")).textContent).toBe("repo busy");
    expect(header()).toBe("2 copies");
  });
});
