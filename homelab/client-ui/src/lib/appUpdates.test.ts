import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import {
  failure,
  forgetFinished,
  settled,
  startUpdate,
  updateNotice,
  useAppUpdate,
} from "./appUpdates";
import { fakeServer, held, never, reply, settle, stream } from "@/test/server";

describe("an update's outcome", () => {
  it("remembers the version a finished update reached", () => {
    expect(settled("update", { ok: true }, "1.2.0")).toEqual({
      kind: "update",
      running: false,
      ok: true,
      version: "1.2.0",
    });
  });

  it("remembers why an update failed", () => {
    expect(settled("settings", { ok: false, error: "no" }, "")).toEqual({
      kind: "settings",
      running: false,
      ok: false,
      error: "no",
    });
  });

  it("names a failure and nothing else", () => {
    expect(failure({ ok: true })).toBeNull();
    expect(failure({ ok: false, error: "no" })).toBe("no");
  });
});

describe("what the service's page says about an update", () => {
  it("says nothing before or while it runs", () => {
    expect(updateNotice(undefined, "Gitea")).toBeNull();
    expect(updateNotice({ kind: "update", running: true }, "Gitea")).toBeNull();
  });

  it("names the version an update reached", () => {
    expect(
      updateNotice(
        { kind: "update", running: false, ok: true, version: "1.2.0" },
        "Gitea",
      ),
    ).toEqual({ tone: "success", title: "Gitea now runs version 1.2.0" });
  });

  it("says it was reinstalled when no version was named", () => {
    expect(
      updateNotice(
        { kind: "update", running: false, ok: true, version: "" },
        "Gitea",
      ),
    ).toEqual({ tone: "success", title: "Gitea was reinstalled" });
  });

  it("says new settings are in place", () => {
    expect(
      updateNotice(
        { kind: "settings", running: false, ok: true, version: "1.0.0" },
        "Gitea",
      ),
    ).toEqual({ tone: "success", title: "Gitea runs with its new settings" });
  });

  it("says why an update or new settings did not go through", () => {
    expect(
      updateNotice(
        { kind: "update", running: false, ok: false, error: "no" },
        "Gitea",
      ),
    ).toEqual({
      tone: "error",
      title: "The update did not finish",
      body: "no",
    });
    expect(
      updateNotice(
        { kind: "settings", running: false, ok: false, error: "no" },
        "Gitea",
      ),
    ).toEqual({
      tone: "error",
      title: "The new settings were not applied",
      body: "no",
    });
  });
});

describe("startUpdate", () => {
  it("answers as soon as the server takes the update, while it still runs", async () => {
    const server = fakeServer({
      "POST /api/apps/gitea/update": held("Updating…"),
    });
    const { result } = renderHook(() => useAppUpdate("gitea"));
    expect(
      await startUpdate("gitea", "settings", { keep_version: true }),
    ).toBeNull();
    expect(result.current).toEqual({ kind: "settings", running: true });
    expect(server.sent("POST", "/api/apps/gitea/update")[0].body).toEqual({
      keep_version: true,
    });
  });

  it("shows it as running before the server answers", () => {
    fakeServer({ "POST /api/apps/gitea/update": never() });
    const { result } = renderHook(() => useAppUpdate("gitea"));
    act(() => void startUpdate("gitea", "update"));
    expect(result.current).toEqual({ kind: "update", running: true });
  });

  it("sends no body when there are no new settings", async () => {
    const server = fakeServer({
      "POST /api/apps/gitea/update": stream("[DONE]"),
    });
    await startUpdate("gitea", "update");
    expect(
      server.sent("POST", "/api/apps/gitea/update")[0].body,
    ).toBeUndefined();
  });

  it("keeps the version the update reached once it finishes", async () => {
    fakeServer({
      "POST /api/apps/gitea/update": stream(
        "Fetching the newest version…",
        "Now on version 1.2.0",
        "[DONE]",
      ),
    });
    const { result } = renderHook(() => useAppUpdate("gitea"));
    await act(() => startUpdate("gitea", "update"));
    await settle();
    expect(result.current).toEqual({
      kind: "update",
      running: false,
      ok: true,
      version: "1.2.0",
    });
  });

  it("keeps why an accepted update failed", async () => {
    fakeServer({
      "POST /api/apps/gitea/update": stream("[ERROR] helm refused"),
    });
    const { result } = renderHook(() => useAppUpdate("gitea"));
    expect(await act(() => startUpdate("gitea", "update"))).toBeNull();
    await settle();
    expect(result.current).toMatchObject({ ok: false, error: "helm refused" });
  });

  it("answers with the reason when the server refuses the update", async () => {
    fakeServer({
      "POST /api/apps/gitea/update": reply(400, '{"error":"bad settings"}'),
    });
    expect(await startUpdate("gitea", "settings", {})).toBe("bad settings");
  });
});

describe("forgetFinished", () => {
  it("forgets a finished update", async () => {
    fakeServer({ "POST /api/apps/gitea/update": stream("[DONE]") });
    const { result } = renderHook(() => useAppUpdate("gitea"));
    await act(() => startUpdate("gitea", "update"));
    await settle();
    act(() => forgetFinished("gitea"));
    expect(result.current).toBeUndefined();
  });

  it("keeps an update that still runs", () => {
    fakeServer({ "POST /api/apps/gitea/update": never() });
    const { result } = renderHook(() => useAppUpdate("gitea"));
    act(() => void startUpdate("gitea", "update"));
    act(() => forgetFinished("gitea"));
    expect(result.current).toEqual({ kind: "update", running: true });
  });

  it("does nothing for a service with no update", () => {
    const { result } = renderHook(() => useAppUpdate("gitea"));
    act(() => forgetFinished("gitea"));
    expect(result.current).toBeUndefined();
  });

  it("stops telling a page that went away", async () => {
    fakeServer({ "POST /api/apps/gitea/update": stream("[DONE]") });
    const { result, unmount } = renderHook(() => useAppUpdate("gitea"));
    unmount();
    await startUpdate("gitea", "update");
    await settle();
    expect(result.current).toBeUndefined();
  });
});
