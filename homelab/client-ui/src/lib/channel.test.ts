import { afterEach, describe, expect, it, vi } from "vitest";
import { switchChannel } from "./channel";

afterEach(() => {
  vi.unstubAllGlobals();
});

function answer(status: number, body: unknown) {
  return vi.fn(async () => new Response(JSON.stringify(body), { status }));
}

describe("switchChannel", () => {
  it("sends the trimmed source and ref and reports no error when the server takes it", async () => {
    const fetch = answer(200, {});
    vi.stubGlobal("fetch", fetch);
    expect(await switchChannel(" github:a/b ", " main ")).toBeNull();
    const [path, init] = fetch.mock.calls[0] as unknown as [string, RequestInit];
    expect(path).toBe("/api/update/channel");
    expect(init.method).toBe("PUT");
    expect(JSON.parse(String(init.body))).toEqual({
      url: "github:a/b",
      ref: "main",
    });
  });

  it("returns the server's reason when it refuses the channel", async () => {
    vi.stubGlobal("fetch", answer(400, { error: "no such ref" }));
    expect(await switchChannel("github:a/b", "nope")).toBe("no such ref");
  });

  it("says the switch failed when the server cannot be reached", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      }),
    );
    expect(await switchChannel("github:a/b", "main")).toBe(
      "Could not switch the channel",
    );
  });
});
