import { act, fireEvent, render, screen } from "@testing-library/react";
import { Outlet } from "react-router-dom";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { api } from "@/lib/api";
import { recall, remember } from "@/lib/localCache";
import { fakeServer, never, reply } from "@/test/server";

const { stub } = vi.hoisted(() => ({
  stub: (name: string) => () => `page ${name}`,
}));

vi.mock("@/components/AppShell", () => ({
  AppShell: ({ onLogout }: { onLogout: () => void }) => (
    <div>
      <button onClick={onLogout}>sign out</button>
      <Outlet />
    </div>
  ),
}));
vi.mock("@/components/RecoveryKeyGate", () => ({
  RecoveryKeyGate: () => null,
}));
vi.mock("@/pages/LoginPage", () => ({
  LoginPage: ({ onLogin }: { onLogin: () => void }) => (
    <button onClick={onLogin}>log in</button>
  ),
}));
vi.mock("@/pages/system/SystemSubPage", () => ({
  SystemSubPage: ({
    title,
    children,
  }: {
    title: string;
    children: ReactNode;
  }) => (
    <section>
      <h1>{title}</h1>
      {children}
    </section>
  ),
}));
vi.mock("@/pages/HomePage", () => ({ HomePage: stub("home") }));
vi.mock("@/pages/AppsPage", () => ({ AppsPage: stub("store") }));
vi.mock("@/pages/InstallPage", () => ({ InstallPage: stub("install") }));
vi.mock("@/pages/CustomAppPage", () => ({ default: stub("custom") }));
vi.mock("@/pages/AppDetailPage", () => ({ AppDetailPage: stub("detail") }));
vi.mock("@/pages/system/SystemPage", () => ({ SystemPage: stub("system") }));
vi.mock("@/pages/system/StoragePage", () => ({ StoragePage: stub("storage") }));
vi.mock("@/pages/system/BackupsPage", () => ({ BackupsPage: stub("backups") }));
vi.mock("@/pages/system/BackupTimelinePage", () => ({
  BackupTimelinePage: stub("timeline"),
}));
vi.mock("@/pages/system/NodesPage", () => ({ NodesPage: stub("machines") }));
vi.mock("@/pages/system/UpdatesPage", () => ({ UpdatesPage: stub("updates") }));
vi.mock("@/pages/system/LogsPage", () => ({ LogsPage: stub("logs") }));
vi.mock("@/pages/system/SettingsPage", () => ({
  SettingsPage: stub("settings"),
}));

beforeEach(() => {
  window.history.replaceState({}, "", "/");
});

const ok = { "GET /api/status": { commit_hash: "", commit_date: "" } };

describe("App", () => {
  it("shows nothing while it checks whether you are signed in", () => {
    fakeServer({ "GET /api/status": never() });
    const { container } = render(<App />);
    expect(container.innerHTML).toBe("");
  });

  it("asks you to sign in, then opens your services", async () => {
    fakeServer({ "GET /api/status": reply(401, "") });
    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "log in" }));
    expect(await screen.findByText("page home")).toBeTruthy();
  });

  it("opens the app when the server answers with another error", async () => {
    fakeServer({ "GET /api/status": reply(500, "") });
    render(<App />);
    expect(await screen.findByText("page home")).toBeTruthy();
  });

  it.each([
    ["/app/gitea", "page detail"],
    ["/add", "page store"],
    ["/add/custom", "page custom"],
    ["/add/gitea", "page install"],
    ["/system", "page system"],
    ["/system/storage", "page storage"],
    ["/system/backups", "page backups"],
    ["/system/backups/yolab-gitea", "page timeline"],
    ["/system/machines", "page machines"],
    ["/system/updates", "page updates"],
    ["/system/settings", "page settings"],
    ["/system/logs", "page logs"],
    ["/box/machines", "page machines"],
    ["/nowhere", "page home"],
  ])("opens %s", async (path, page) => {
    window.history.replaceState({}, "", path);
    fakeServer(ok);
    render(<App />);
    expect(await screen.findByText(page)).toBeTruthy();
  });

  it("goes back to sign in and forgets what it remembered when signed out", async () => {
    fakeServer(ok);
    remember("apps", [1], 1);
    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: "sign out" }));
    expect(screen.getByRole("button", { name: "log in" })).toBeTruthy();
    expect(recall("apps")).toBeNull();
  });

  it("goes back to sign in when the server says the session ended", async () => {
    const server = fakeServer(ok);
    const { unmount } = render(<App />);
    await screen.findByText("page home");
    server.routes["GET /api/apps"] = reply(401, "");
    await act(() => api.get("/api/apps").catch(() => undefined));
    expect(screen.getByRole("button", { name: "log in" })).toBeTruthy();
    unmount();
    await api.get("/api/apps").catch(() => undefined);
  });
});
