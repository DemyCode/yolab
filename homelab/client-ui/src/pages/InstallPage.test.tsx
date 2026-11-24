import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { InstallPage } from "./InstallPage";
import { fakeServer, never, reply, settle, stream } from "@/test/server";
import { renderAt } from "@/test/render";
import { appInfo, catalogApp } from "@/test/fixtures";
import type { CatalogApp } from "@/types/apps";

const schema = {
  type: "object",
  required: ["site"],
  properties: {
    site: { type: "string", title: "Site title" },
    note: { type: "string", title: "Note", default: "" },
    extra: { type: "string", title: "Extra" },
  },
};

const gitea = (over: Partial<CatalogApp> = {}) =>
  catalogApp({
    id: "gitea",
    name: "Gitea",
    icon: "🐙",
    tagline: "Git at home",
    description: "Hosts your code",
    schema,
    ...over,
  });

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
}

function serve(over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/apps/catalog": [gitea()],
    "GET /api/tunnel/domain": { domain: "me.yolab.io" },
    "GET /api/apps": [],
    "GET /api/store/stats": [],
    "POST /api/apps/catalog/gitea/refresh": { app: null },
    "GET /api/store/apps/gitea/heart": never(),
    "GET /api/store/apps/gitea/comments": never(),
    ...over,
  });
}

const open = (query = "") =>
  renderAt(<InstallPage />, {
    path: `/add/gitea${query}`,
    route: "/add/:appId",
  });

const where = () => screen.getByTestId("where").textContent;
const blocker = () =>
  document.querySelector("p[role=status]")?.textContent ?? null;
const go = () =>
  screen.getByRole("button", {
    name: /^(Install|Add|Replace|Apply|Update|Starting|Applying|Updating)/,
  });

beforeEach(() => {
  window.scrollTo = vi.fn() as unknown as typeof window.scrollTo;
});

describe("InstallPage, before the service is known", () => {
  it("waits for the catalog", () => {
    serve({
      "GET /api/apps/catalog": never(),
      "POST /api/apps/catalog/gitea/refresh": never(),
    });
    const { container } = open();
    expect(container.querySelector("svg.animate-spin")).not.toBeNull();
  });

  it("says when the service is not in the catalog", async () => {
    serve({ "GET /api/apps/catalog": [] });
    open();
    expect(
      await screen.findByText("That service is not in the catalog any more."),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "Back to services" }));
    expect(where()).toBe("/add");
  });

  it("says why the catalog could not be asked", async () => {
    serve({
      "GET /api/apps/catalog": [],
      "POST /api/apps/catalog/gitea/refresh": reply(
        502,
        '{"error":"catalog offline"}',
      ),
    });
    open();
    expect(await screen.findByText("catalog offline")).toBeTruthy();
  });

  it("names the failure when the catalog did not answer", async () => {
    serve({
      "GET /api/apps/catalog": [],
      "POST /api/apps/catalog/gitea/refresh": () => Promise.reject("x"),
    });
    open();
    expect(await screen.findByText("The catalog did not answer.")).toBeTruthy();
  });

  it("ignores catalog answers that come after the page went away", async () => {
    const ok = deferred<unknown>();
    serve({ "POST /api/apps/catalog/gitea/refresh": () => ok.promise });
    open().unmount();
    ok.resolve({ app: gitea({ name: "Late" }) });
    const bad = deferred<unknown>();
    serve({ "POST /api/apps/catalog/gitea/refresh": () => bad.promise });
    open().unmount();
    bad.reject(new Error("x"));
    await settle();
    expect(document.body.textContent).toBe("");
  });
});

describe("InstallPage, the overview", () => {
  it("presents an official service with its facts, and the newest catalog entry", async () => {
    serve({
      "POST /api/apps/catalog/gitea/refresh": {
        app: gitea({
          name: "Gitea Fresh",
          github: "go-gitea/gitea",
          stars: 45000,
          home: "https://gitea.io",
          pushed_at: new Date().toISOString(),
        }),
      },
      "GET /api/store/stats": [
        { app_id: "gitea", installs: 1, hearts: 2, comment_count: 0 },
      ],
    });
    open();
    expect(
      await screen.findByRole("heading", { name: "Gitea Fresh" }),
    ).toBeTruthy();
    expect(screen.getByText("Git at home")).toBeTruthy();
    expect(screen.getByText("Hosts your code")).toBeTruthy();
    expect(screen.getByText("install")).toBeTruthy();
    expect(screen.getByText("45k")).toBeTruthy();
    expect(screen.getByText("Updated today")).toBeTruthy();
    expect(
      screen
        .getByRole("link", { name: /Project website/ })
        .getAttribute("href"),
    ).toBe("https://gitea.io");
    expect(screen.queryByText("You already run")).toBeNull();
    fireEvent.click(screen.getByRole("link", { name: "All services" }));
    expect(where()).toBe("/add");
  });

  it("links the source code without stars, hides a website that is the repository, counts installs", async () => {
    serve({
      "GET /api/apps/catalog": [
        gitea({
          github: "go-gitea/gitea",
          stars: 0,
          home: "https://github.com/go-gitea/gitea",
          description: "Git at home",
          collections: ["start-here"],
        }),
        gitea({ id: "forgejo", name: "Forgejo", collections: ["start-here"] }),
      ],
      "GET /api/store/stats": [
        { app_id: "gitea", installs: 3, hearts: 0, comment_count: 0 },
      ],
      "GET /api/apps": [
        appInfo({ instance_name: "gitea", detail: "Running" }),
        appInfo({ app_id: "forgejo", instance_name: "forgejo" }),
      ],
    });
    open();
    expect(await screen.findByText("Source code")).toBeTruthy();
    expect(screen.getByText("installs")).toBeTruthy();
    expect(screen.queryByText("Project website")).toBeNull();
    expect(screen.queryByText("What it does")).toBeNull();
    expect(
      screen.getByText("Installing again adds a separate copy"),
    ).toBeTruthy();
    expect(screen.getByText("Similar services")).toBeTruthy();
    expect(screen.getByText("Forgejo")).toBeTruthy();
    fireEvent.click(
      within(screen.getByText("You already run").closest("section")!).getByText(
        "gitea",
      ),
    );
    expect(where()).toBe("/app/gitea");
  });

  it("warns about a service from another source, without its community", async () => {
    serve({
      "GET /api/apps/catalog": [
        gitea({ repo: "friends", github: "", stars: null }),
      ],
    });
    open();
    expect(
      await screen.findByText('This service comes from "friends"'),
    ).toBeTruthy();
    expect(screen.queryByText(/installs?$/)).toBeNull();
    expect(screen.queryByText("Source code")).toBeNull();
  });

  it("opens the settings, and goes back to the overview", async () => {
    serve();
    open();
    fireEvent.click(
      await screen.findByRole("button", { name: "Install Gitea" }),
    );
    expect(screen.getByRole("heading", { name: "Install Gitea" })).toBeTruthy();
    expect(
      screen.getByText(
        "Choose how it is set up. You can change these settings later.",
      ),
    ).toBeTruthy();
    expect(window.scrollTo).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Gitea" }));
    expect(screen.getByRole("heading", { name: "Gitea" })).toBeTruthy();
  });
});

describe("InstallPage, a fresh install", () => {
  it("needs the required settings, then installs and opens the service", async () => {
    const server = serve({
      "POST /api/apps/gitea": { instance_name: "gitea-x1y2" },
    });
    open("?step=settings");
    await screen.findByLabelText(/Site title/);
    expect(blocker()).toBe("Fill in everything marked required.");
    expect(go()).toHaveProperty("disabled", true);
    fireEvent.change(screen.getByLabelText(/Site title/), {
      target: { value: "Code" },
    });
    fireEvent.change(screen.getByLabelText(/Extra/), {
      target: { value: "x" },
    });
    fireEvent.change(screen.getByLabelText(/Extra/), { target: { value: "" } });
    await waitFor(() => expect(blocker()).toBeNull());
    fireEvent.click(go());
    await waitFor(() => expect(where()).toBe("/app/gitea-x1y2"));
    expect(server.sent("POST", "/api/apps/gitea")[0].body).toEqual({
      instance_name: "gitea",
      config: { site: "Code", note: "" },
    });
  });

  it("shows the passwords it made before leaving", async () => {
    serve({
      "GET /api/apps/catalog": [
        gitea({
          schema: {
            type: "object",
            properties: {
              password: {
                type: "string",
                title: "Admin password",
                writeOnly: true,
                generate: true,
              },
            },
          },
        }),
      ],
      "POST /api/apps/gitea": { instance_name: "gitea-x1y2" },
    });
    open("?step=settings");
    await waitFor(() => expect(blocker()).toBeNull());
    fireEvent.click(go());
    expect(
      await screen.findByRole("heading", { name: "Installing Gitea" }),
    ).toBeTruthy();
    expect(screen.getByText("Admin password")).toBeTruthy();
    expect(
      document.querySelector("code")!.textContent!.length,
    ).toBeGreaterThanOrEqual(24);
    fireEvent.click(screen.getByRole("button", { name: "Go to Gitea" }));
    expect(where()).toBe("/app/gitea-x1y2");
  });

  it("says why the install did not start", async () => {
    const server = serve({
      "POST /api/apps/gitea": reply(409, '{"error":"name taken"}'),
    });
    open("?step=settings");
    fireEvent.change(await screen.findByLabelText(/Site title/), {
      target: { value: "Code" },
    });
    await waitFor(() => expect(blocker()).toBeNull());
    fireEvent.click(go());
    expect(await screen.findByText("name taken")).toBeTruthy();
    expect(screen.getByText("The install could not start")).toBeTruthy();
    server.routes["POST /api/apps/gitea"] = () => Promise.reject("x");
    fireEvent.click(go());
    expect(
      await screen.findByText("The install could not start."),
    ).toBeTruthy();
  });

  it("checks the folder it is given in the file tree", async () => {
    serve({
      "GET /api/apps/catalog": [
        gitea({
          schema: {
            type: "object",
            properties: {
              files_folder: {
                type: "string",
                title: "Files folder",
                format: "tree-folder",
              },
            },
          },
        }),
      ],
      "GET /api/tree/folders?path=": {
        path: "",
        folders: ["books"],
        truncated: false,
      },
      "GET /api/tree/folders?path=books": {
        path: "books",
        folders: [],
        truncated: false,
      },
    });
    open("?step=settings");
    expect(await screen.findByText("Pick the files folder.")).toBeTruthy();
    fireEvent.mouseDown(await screen.findByRole("option", { name: /books/ }));
    fireEvent.click(await screen.findByRole("button", { name: /^Use / }));
    await waitFor(() => expect(blocker()).toBeNull());
  });
});

const def = (over: Record<string, unknown> = {}) => ({
  app_id: "gitea",
  instance_name: "gitea-ab12",
  chart_version: "1.0.0",
  config: { site: "Old site" },
  ...over,
});

describe("InstallPage, duplicating a service", () => {
  it("starts from its settings, with or without its files", async () => {
    const server = serve({
      "GET /api/apps/gitea-ab12/definition": def(),
      "POST /api/apps/gitea": { instance_name: "gitea-cd34" },
    });
    open("?from=gitea-ab12");
    expect(
      await screen.findByRole("heading", { name: "Duplicate gitea-ab12" }),
    ).toBeTruthy();
    await waitFor(() =>
      expect(
        (screen.getByLabelText(/Site title/) as HTMLInputElement).value,
      ).toBe("Old site"),
    );
    expect(screen.getByText("It starts empty.")).toBeTruthy();
    fireEvent.click(screen.getByRole("switch"));
    expect(
      screen.getByText("Copied straight from the service, as they are now."),
    ).toBeTruthy();
    fireEvent.click(go());
    await waitFor(() => expect(where()).toBe("/app/gitea-cd34"));
    expect(server.sent("POST", "/api/apps/gitea")[0].body).toMatchObject({
      source: {
        kind: "duplicate",
        from_instance: "gitea-ab12",
        with_data: true,
      },
    });
  });

  it("goes back to the store, and names the source until it is read", async () => {
    serve({ "GET /api/apps/gitea-ab12/definition": never() });
    open("?from=gitea-ab12");
    expect(
      await screen.findByRole("heading", { name: "Duplicate gitea-ab12" }),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "All services" }));
    expect(where()).toBe("/add");
  });

  it("says whether its files folder is shared or copied into another one", async () => {
    serve({
      "GET /api/apps/catalog": [
        gitea({
          schema: {
            type: "object",
            properties: {
              files_folder_existing: { type: "boolean", title: "Use a folder" },
              files_folder: {
                type: "string",
                title: "Files folder",
                format: "tree-folder",
              },
            },
          },
        }),
      ],
      "GET /api/apps/gitea-ab12/definition": def({
        config: { files_folder_existing: true, files_folder: "photos" },
      }),
      "GET /api/tree/folders?path=": {
        path: "",
        folders: ["photos", "books"],
        truncated: false,
      },
      "GET /api/tree/folders?path=books": {
        path: "books",
        folders: [],
        truncated: false,
      },
    });
    open("?from=gitea-ab12");
    fireEvent.click(
      await screen.findByRole("switch", {
        name: "Copy this service’s files too",
      }),
    );
    expect(
      await screen.findByText(/It shares \/photos with the original/),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "/" }));
    fireEvent.mouseDown(await screen.findByRole("option", { name: /books/ }));
    fireEvent.click(await screen.findByRole("button", { name: /^Use / }));
    expect(
      await screen.findByText(/The files are copied into \/books/),
    ).toBeTruthy();
  });

  it("says why its settings could not be read", async () => {
    serve({
      "GET /api/apps/gitea-ab12/definition": reply(404, '{"error":"gone"}'),
    });
    open("?from=gitea-ab12");
    expect(
      await screen.findByText("Its settings could not be read: gone"),
    ).toBeTruthy();
    expect(go()).toHaveProperty("disabled", true);
  });

  it("names the failure when the server did not answer", async () => {
    serve({ "GET /api/apps/gitea-ab12/definition": () => Promise.reject("x") });
    open("?from=gitea-ab12");
    expect(
      await screen.findByText(
        "Its settings could not be read: the server did not answer",
      ),
    ).toBeTruthy();
  });
});

const NS = "yolab-gitea";
const plan = { data: [{ claim: "data", capacity: "1Gi", backed_up: true }] };

function restoring(over: Record<string, unknown> = {}) {
  return serve({
    [`GET /api/backups/apps/${NS}/definition?snapshot_id=s1`]: def({
      instance_name: "gitea",
    }),
    [`GET /api/backups/apps/${NS}/points`]: {
      points: [{ snapshot_id: "s1", time: "2026-05-06T07:08:00Z" }],
    },
    [`GET /api/backups/apps/${NS}/plan?snapshot_id=s1`]: plan,
    ...over,
  });
}

describe("InstallPage, restoring a removed service", () => {
  it("adds it back from the copy, with its settings and data", async () => {
    const server = restoring({
      "POST /api/apps/gitea": { instance_name: "gitea" },
    });
    open(`?restore=${NS}&snapshot=s1`);
    expect(
      await screen.findByRole("heading", { name: "Restore gitea" }),
    ).toBeTruthy();
    expect(screen.queryByText("Where it goes")).toBeNull();
    expect(screen.getByText(/A new service from this copy/)).toBeTruthy();
    expect(await screen.findByText(/2026/)).toBeTruthy();
    expect(
      screen
        .getByRole("link", { name: "Pick another moment" })
        .getAttribute("href"),
    ).toBe(`/system/backups/${NS}`);
    await waitFor(() => expect(blocker()).toBeNull());
    expect(go().textContent).toBe("Add Gitea from this copy");
    fireEvent.click(go());
    await waitFor(() => expect(where()).toBe("/app/gitea"));
    expect(server.sent("POST", "/api/apps/gitea")[0].body).toMatchObject({
      instance_name: "gitea",
      source: {
        kind: "backup",
        namespace: NS,
        snapshot_id: "s1",
        with_data: true,
        choices: { data: { data: "restore" } },
      },
    });
  });

  it("shows the copy's id until its time is known, and installs before the plan is read", async () => {
    const server = restoring({
      [`GET /api/backups/apps/${NS}/points`]: never(),
      [`GET /api/backups/apps/${NS}/plan?snapshot_id=s1`]: never(),
      "POST /api/apps/gitea": { instance_name: "gitea" },
    });
    open(`?restore=${NS}&snapshot=s1`);
    expect(await screen.findByText("s1")).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "Its backups" }));
    expect(where()).toBe(`/system/backups/${NS}`);
    await settle();
    expect(server.sent("POST", "/api/apps/gitea")).toHaveLength(0);
  });

  it("installs from the copy when its data plan is not read yet", async () => {
    const server = restoring({
      [`GET /api/backups/apps/${NS}/plan?snapshot_id=s1`]: never(),
      "POST /api/apps/gitea": { instance_name: "gitea" },
    });
    open(`?restore=${NS}&snapshot=s1`);
    await screen.findByRole("heading", { name: "Restore gitea" });
    await waitFor(() => expect(blocker()).toBeNull());
    fireEvent.click(go());
    await waitFor(() =>
      expect(server.sent("POST", "/api/apps/gitea")).toHaveLength(1),
    );
    expect(server.sent("POST", "/api/apps/gitea")[0].body).not.toHaveProperty(
      "source.choices",
    );
  });
});

describe("InstallPage, restoring a service that still runs", () => {
  const runs = { "GET /api/apps": [appInfo({ instance_name: "gitea" })] };

  it("replaces it in place once the copy is read", async () => {
    const server = restoring({
      ...runs,
      "POST /api/backups/restore": undefined,
    });
    open(`?restore=${NS}&snapshot=s1`);
    expect(await screen.findByText("Where it goes")).toBeTruthy();
    expect(
      screen.getByText(/gitea goes back to how it was in this copy/),
    ).toBeTruthy();
    expect(screen.queryByText("Settings")).toBeNull();
    expect(screen.getByText("What comes back")).toBeTruthy();
    await waitFor(() => expect(blocker()).toBeNull());
    expect(go().textContent).toBe("Replace gitea with this copy");
    fireEvent.click(go());
    await waitFor(() =>
      expect(server.sent("POST", "/api/backups/restore")).toHaveLength(1),
    );
    expect(server.sent("POST", "/api/backups/restore")[0].body).toEqual({
      namespace: NS,
      snapshot_id: "s1",
      choices: { data: { data: "replace" } },
    });
    expect(where()).toBe("/app/gitea");
  });

  it("waits for the copy, then says why it could not be read", async () => {
    const reading = deferred<Response>();
    restoring({
      ...runs,
      [`GET /api/backups/apps/${NS}/plan?snapshot_id=s1`]: () =>
        reading.promise,
    });
    open(`?restore=${NS}&snapshot=s1`);
    await screen.findByText("Where it goes");
    await waitFor(() => expect(blocker()).toBe("Reading the backup…"));
    reading.resolve(new Response('{"error":"repo locked"}', { status: 500 }));
    await waitFor(() => expect(blocker()).toBe("repo locked"));
  });

  it("says why the restore did not start", async () => {
    const server = restoring({
      ...runs,
      "POST /api/backups/restore": reply(409, '{"error":"busy"}'),
    });
    open(`?restore=${NS}&snapshot=s1`);
    await waitFor(() => expect(blocker()).toBeNull());
    fireEvent.click(go());
    expect(await screen.findByText("busy")).toBeTruthy();
    server.routes["POST /api/backups/restore"] = () => Promise.reject("x");
    fireEvent.click(go());
    expect(await screen.findByText("The restore did not start.")).toBeTruthy();
  });

  it("can add it next to the running one instead", async () => {
    restoring(runs);
    open(`?restore=${NS}&snapshot=s1`);
    const radios = await screen.findAllByRole("radio");
    expect(radios.map((r) => (r as HTMLInputElement).checked)).toEqual([
      true,
      false,
    ]);
    fireEvent.click(radios[1]);
    expect(screen.getByText("Settings")).toBeTruthy();
    expect(screen.getByText(/Added next to gitea/)).toBeTruthy();
    expect(go().textContent).toBe("Add Gitea from this copy");
  });

  it("waits for the running services before filling in the settings", async () => {
    restoring({ "GET /api/apps": never() });
    open(`?restore=${NS}&snapshot=s1`);
    await screen.findByRole("heading", { name: "Restore gitea" });
    await settle();
    expect(
      (screen.getByLabelText(/Site title/) as HTMLInputElement).value,
    ).toBe("");
  });
});

describe("InstallPage, changing a running service", () => {
  const editing = (over: Record<string, unknown> = {}) =>
    serve({
      "GET /api/apps/gitea-ab12/definition": def(),
      "GET /api/apps/gitea-ab12/schema": schema,
      ...over,
    });

  it("applies new settings to it and opens it", async () => {
    const server = editing({
      "POST /api/apps/gitea-ab12/update": stream("applying", "[DONE]"),
    });
    open("?edit=gitea-ab12");
    expect(
      await screen.findByRole("heading", { name: "Settings of gitea-ab12" }),
    ).toBeTruthy();
    expect(screen.getByText(/Applied to the version it runs now/)).toBeTruthy();
    await waitFor(() => expect(blocker()).toBeNull());
    expect(go().textContent).toBe("Apply these settings");
    fireEvent.click(go());
    expect(screen.getByRole("button", { name: "Applying…" })).toBeTruthy();
    await waitFor(() => expect(where()).toBe("/app/gitea-ab12"));
    expect(server.sent("POST", "/api/apps/gitea-ab12/update")[0].body).toEqual({
      config: { site: "Old site", note: "" },
      keep_version: true,
    });
  });

  it("names the service after the app until it is read, and links back to it", async () => {
    editing({
      "GET /api/apps/gitea-ab12/definition": never(),
      "GET /api/apps/gitea-ab12/schema": never(),
    });
    open("?edit=gitea-ab12");
    expect(
      await screen.findByRole("heading", { name: "Settings of Gitea" }),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "Gitea" }));
    expect(where()).toBe("/app/gitea-ab12");
  });

  it("says why the settings were not applied", async () => {
    editing({
      "POST /api/apps/gitea-ab12/update": stream("[ERROR] helm refused"),
    });
    open("?edit=gitea-ab12");
    await screen.findByRole("heading", { name: "Settings of gitea-ab12" });
    await waitFor(() => expect(blocker()).toBeNull());
    fireEvent.click(go());
    expect(await screen.findByText("helm refused")).toBeTruthy();
    expect(screen.getByText("The new settings were not applied")).toBeTruthy();
  });

  it("says why its settings form could not be read", async () => {
    editing({ "GET /api/apps/gitea-ab12/schema": () => Promise.reject("x") });
    open("?edit=gitea-ab12");
    expect(
      await screen.findByText(
        "Its settings form could not be read: the server did not answer",
      ),
    ).toBeTruthy();
  });

  it("ignores answers that come after the page went away", async () => {
    const d = deferred<unknown>();
    const s = deferred<unknown>();
    editing({
      "GET /api/apps/gitea-ab12/definition": () => d.promise,
      "GET /api/apps/gitea-ab12/schema": () => s.promise,
    });
    open("?edit=gitea-ab12").unmount();
    d.resolve(def());
    s.resolve(schema);
    const d2 = deferred<unknown>();
    const s2 = deferred<unknown>();
    editing({
      "GET /api/apps/gitea-ab12/definition": () => d2.promise,
      "GET /api/apps/gitea-ab12/schema": () => s2.promise,
    });
    open("?edit=gitea-ab12").unmount();
    d2.reject(new Error("x"));
    s2.reject(new Error("x"));
    await settle();
    expect(document.body.textContent).toBe("");
  });

  it("updates it to the newest version", async () => {
    const server = editing({
      "POST /api/apps/gitea-ab12/update": stream("[DONE]"),
    });
    open("?update=gitea-ab12");
    expect(
      await screen.findByRole("heading", {
        name: "Update gitea-ab12 to 1.0.0",
      }),
    ).toBeTruthy();
    expect(screen.getByText(/The settings of the new version/)).toBeTruthy();
    await waitFor(() => expect(blocker()).toBeNull());
    expect(go().textContent).toBe("Update to 1.0.0");
    fireEvent.click(go());
    expect(screen.getByRole("button", { name: "Updating…" })).toBeTruthy();
    await waitFor(() => expect(where()).toBe("/app/gitea-ab12"));
    expect(
      server.sent("POST", "/api/apps/gitea-ab12/update")[0].body,
    ).toMatchObject({ keep_version: false });
    expect(server.sent("GET", "/api/apps/gitea-ab12/schema")).toHaveLength(0);
  });

  it("says why the update did not finish", async () => {
    editing({
      "POST /api/apps/gitea-ab12/update": reply(
        500,
        '{"error":"chart broken"}',
      ),
    });
    open("?update=gitea-ab12");
    await screen.findByRole("heading", { name: "Update gitea-ab12 to 1.0.0" });
    await waitFor(() => expect(blocker()).toBeNull());
    fireEvent.click(go());
    expect(await screen.findByText("chart broken")).toBeTruthy();
    expect(screen.getByText("The update did not finish")).toBeTruthy();
  });
});

describe("InstallPage, while things arrive", () => {
  it("keeps what you typed when the newest catalog entry arrives", async () => {
    const refreshed = deferred<unknown>();
    serve({ "POST /api/apps/catalog/gitea/refresh": () => refreshed.promise });
    open("?step=settings");
    fireEvent.change(await screen.findByLabelText(/Site title/), {
      target: { value: "Mine" },
    });
    refreshed.resolve({ app: gitea({ name: "Gitea 2" }) });
    expect(
      await screen.findByRole("heading", { name: "Install Gitea 2" }),
    ).toBeTruthy();
    expect(
      (screen.getByLabelText(/Site title/) as HTMLInputElement).value,
    ).toBe("Mine");
  });

  it("names a restore after its backup until the copy is read", async () => {
    restoring({
      [`GET /api/backups/apps/${NS}/definition?snapshot_id=s1`]: never(),
    });
    open(`?restore=${NS}&snapshot=s1`);
    expect(
      await screen.findByRole("heading", { name: `Restore ${NS}` }),
    ).toBeTruthy();
  });
});
