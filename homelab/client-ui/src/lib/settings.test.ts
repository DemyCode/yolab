import { afterEach, describe, expect, it, vi } from "vitest";
import {
  DEFAULT_SPACE_BYTES,
  isPreset,
  saveSettings,
  SETTINGS_PATH,
} from "./settings";

afterEach(() => {
  vi.unstubAllGlobals();
});

function answer(status: number, body: unknown) {
  return vi.fn(async () => new Response(JSON.stringify(body), { status }));
}

const monthly10 = {
  backup_schedule: "0 3 1 * *",
  backup_budget_bytes: 10 * 2 ** 30,
};

describe("saveSettings", () => {
  it("puts the whole set of cluster settings and hands back what the server kept", async () => {
    const fetch = answer(200, monthly10);
    vi.stubGlobal("fetch", fetch);
    expect(await saveSettings(monthly10)).toEqual({
      ok: true,
      settings: monthly10,
    });
    const [path, init] = fetch.mock.calls[0] as unknown as [
      string,
      RequestInit,
    ];
    expect(path).toBe(SETTINGS_PATH);
    expect(init.method).toBe("PUT");
    expect(JSON.parse(String(init.body))).toEqual(monthly10);
  });

  it("returns the server's reason when it refuses a setting", async () => {
    vi.stubGlobal(
      "fetch",
      answer(400, { error: "That backup schedule is not valid" }),
    );
    expect(await saveSettings(monthly10)).toEqual({
      ok: false,
      error: "That backup schedule is not valid",
    });
  });

  it("says the settings were not saved when the server cannot be reached", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      }),
    );
    const saved = await saveSettings(monthly10);
    expect(saved.ok).toBe(false);
  });
});

describe("isPreset", () => {
  it("knows the monthly schedule is one of the choices offered", () => {
    expect(isPreset("0 3 1 * *")).toBe(true);
  });

  it("treats any other cron line as a custom schedule", () => {
    expect(isPreset("15 4 * * 2")).toBe(false);
  });
});

describe("DEFAULT_SPACE_BYTES", () => {
  it("is the 10 GB a service gets when it has no space of its own", () => {
    expect(DEFAULT_SPACE_BYTES).toBe(10_000_000_000);
  });
});
