import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { BackupsPage } from "./BackupsPage";
import { fakeServer, never, reply } from "@/test/server";
import { renderAt } from "@/test/render";
import { catalogApp } from "@/test/fixtures";
import type { BackedUpService, ProtectedApp } from "@/lib/backups";

const app = (over: Partial<ProtectedApp> = {}): ProtectedApp => ({
  namespace: "app-gitea",
  instance_name: "gitea",
  app_id: "gitea",
  enabled: true,
  schedule: "0 3 * * *",
  state: "ok",
  last_ok_at: new Date().toISOString(),
  error: null,
  size_bytes: 2048,
  ...over,
});

const gone = (over: Partial<BackedUpService> = {}): BackedUpService => ({
  namespace: "app-old",
  instance_name: "old",
  installed: false,
  versions: [{ snapshot_id: "s1", time: "2026-01-01T00:00:00Z" }],
  size_bytes: null,
  ...over,
});

function serve(over: Record<string, unknown> = {}) {
  return fakeServer({
    "GET /api/backups/protected": { configured: true, apps: [] },
    "GET /api/backups/apps": { configured: true, apps: [] },
    "GET /api/apps/catalog": [],
    ...over,
  });
}

const where = () => screen.getByTestId("where").textContent;

describe("BackupsPage", () => {
  it("shows a placeholder while it loads", () => {
    serve({ "GET /api/backups/protected": never() });
    const { container } = renderAt(<BackupsPage />);
    expect(container.querySelector(".shimmer")).not.toBeNull();
    expect(
      screen.getByText(/Every service is copied with its own data/),
    ).toBeTruthy();
  });

  it("turns backups on and makes you save the recovery key", async () => {
    let on = false;
    const server = serve({
      "GET /api/backups/protected": () => ({ configured: on, apps: [] }),
      "POST /api/backups/s3/enable": () => {
        on = true;
        return undefined;
      },
      "GET /api/backups/recovery-key": {
        configured: true,
        recovery_key: "KEY-42",
      },
      "POST /api/backups/recovery-key/ack": undefined,
    });
    renderAt(<BackupsPage />);
    expect(await screen.findByText("Backups are off")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Turn on backups" }));
    expect(await screen.findByText("KEY-42")).toBeTruthy();
    expect(screen.getByText("Nothing to back up yet")).toBeTruthy();
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: /continue/ }));
    await waitFor(() => expect(screen.queryByText("KEY-42")).toBeNull());
    expect(server.sent("POST", "/api/backups/recovery-key/ack")).toHaveLength(
      1,
    );
  });

  it("says why backups were not turned on", async () => {
    const server = serve({
      "GET /api/backups/protected": { configured: false, apps: [] },
      "POST /api/backups/s3/enable": reply(502, '{"error":"no bucket"}'),
    });
    renderAt(<BackupsPage />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Turn on backups" }),
    );
    expect(await screen.findByText("no bucket")).toBeTruthy();
    expect(screen.getByText("That did not work")).toBeTruthy();
    server.routes["POST /api/backups/s3/enable"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Turn on backups" }));
    expect(
      await screen.findByText("Backups could not be turned on."),
    ).toBeTruthy();
  });

  it("lists services biggest first, removed ones apart, and opens each", async () => {
    serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [
          app(),
          app({
            namespace: "app-big",
            instance_name: "big",
            app_id: "zz",
            size_bytes: 9e9,
          }),
          app({
            namespace: "app-none",
            instance_name: "none",
            app_id: "nothing",
            size_bytes: null,
          }),
        ],
      },
      "GET /api/backups/apps": { configured: true, apps: [gone()] },
      "GET /api/apps/catalog": [catalogApp({ id: "zz", icon: "🐙" })],
    });
    renderAt(<BackupsPage />);
    expect(await screen.findByText("Removed services")).toBeTruthy();
    const names = [
      ...document.querySelectorAll("span.block.truncate:first-child"),
    ].map((s) => s.textContent);
    expect(names).toEqual(["big", "gitea", "none", "old"]);
    expect(screen.getByText("🐙")).toBeTruthy();
    expect(screen.getAllByText("—")).toHaveLength(2);
    expect(
      screen.getByText(/of encrypted copies, kept outside your home/),
    ).toBeTruthy();
    fireEvent.click(screen.getByText("old"));
    expect(where()).toBe("/system/backups/app-old");
  });

  it("marks failing, running and stale services", async () => {
    serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [
          app({
            namespace: "a",
            instance_name: "a",
            state: "failed",
            error: "boom",
          }),
          app({
            namespace: "b",
            instance_name: "b",
            state: "running",
            last_ok_at: "2020-01-01T00:00:00Z",
          }),
        ],
      },
    });
    const { container } = renderAt(<BackupsPage />);
    expect(await screen.findByText(/services have no/)).toBeTruthy();
    expect(screen.getByText(/each one/)).toBeTruthy();
    expect(
      container.querySelectorAll(".animate-pulse, [class*=pulse]").length,
    ).toBeGreaterThan(0);
  });

  it("names the one service without a recent backup", async () => {
    serve({
      "GET /api/backups/protected": {
        configured: true,
        apps: [app({ state: "never", last_ok_at: null })],
      },
    });
    renderAt(<BackupsPage />);
    expect(await screen.findByText("gitea has no recent backup")).toBeTruthy();
    expect(screen.getByText(/Open it below/)).toBeTruthy();
  });

  it("goes to the defaults, and shows the recovery key on request", async () => {
    serve({
      "GET /api/backups/recovery-key": {
        configured: true,
        recovery_key: "K-7",
      },
    });
    renderAt(<BackupsPage />);
    fireEvent.click(await screen.findByRole("button", { name: "Show" }));
    expect(await screen.findByText("K-7")).toBeTruthy();
    expect(screen.queryByRole("checkbox")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(screen.queryByText("K-7")).toBeNull();
    fireEvent.click(screen.getByText("Defaults for new services"));
    expect(where()).toBe("/system/settings");
  });

  it("says when there is no recovery key, or it cannot be read", async () => {
    const server = serve({
      "GET /api/backups/recovery-key": { configured: false },
    });
    renderAt(<BackupsPage />);
    fireEvent.click(await screen.findByRole("button", { name: "Show" }));
    expect(
      await screen.findByText("There is no recovery key on this server yet."),
    ).toBeTruthy();
    server.routes["GET /api/backups/recovery-key"] = reply(
      500,
      '{"error":"locked"}',
    );
    fireEvent.click(screen.getByRole("button", { name: "Show" }));
    expect(await screen.findByText("locked")).toBeTruthy();
    server.routes["GET /api/backups/recovery-key"] = () => Promise.reject("x");
    fireEvent.click(screen.getByRole("button", { name: "Show" }));
    expect(
      await screen.findByText("The recovery key could not be read."),
    ).toBeTruthy();
  });
});
