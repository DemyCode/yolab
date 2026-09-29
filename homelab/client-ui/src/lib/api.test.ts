import { afterEach, describe, expect, it, vi } from "vitest";
import { streamEvents } from "./api";

type Read = () => Promise<{ done: boolean; value?: Uint8Array }>;

function streaming(read: Read) {
  return {
    status: 200,
    ok: true,
    body: { getReader: () => ({ read }) },
    text: async () => "",
  };
}

function frames(...lines: string[]): Read {
  const chunks = lines.map((l) => new TextEncoder().encode(`data: ${l}\n\n`));
  return async () => {
    const value = chunks.shift();
    return value ? { done: false, value } : { done: true };
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("streamEvents", () => {
  it("reports a finished run as done", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        streaming(frames("Installing…", "[DONE] gitea installed")),
      ),
    );
    const result = await streamEvents("/api/apps/gitea", {}, () => {});
    expect(result.ok).toBe(true);
    expect(result.dropped).toBeUndefined();
  });

  it("reports the box's own failure as a failure, not as a dropped connection", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        streaming(frames("Installing…", "[ERROR] helm said no")),
      ),
    );
    const result = await streamEvents("/api/apps/gitea", {}, () => {});
    expect(result).toEqual({ ok: false, error: "helm said no" });
  });

  it("treats a stream that ends without a verdict as dropped, because the box keeps going", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => streaming(frames("Installing…"))));
    const result = await streamEvents("/api/apps/gitea", {}, () => {});
    expect(result.ok).toBe(false);
    expect(result.dropped).toBe(true);
  });

  it("treats a connection that breaks mid-stream as dropped instead of throwing", async () => {
    let first = true;
    const read: Read = async () => {
      if (first) {
        first = false;
        return {
          done: false,
          value: new TextEncoder().encode("data: Installing…\n\n"),
        };
      }
      throw new TypeError("network error");
    };
    vi.stubGlobal("fetch", vi.fn(async () => streaming(read)));
    const lines: string[] = [];
    const result = await streamEvents("/api/apps/gitea", {}, (l) =>
      lines.push(l),
    );
    expect(lines).toEqual(["Installing…"]);
    expect(result.dropped).toBe(true);
  });

  it("treats a request that never got through as dropped instead of throwing", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("Failed to fetch");
      }),
    );
    const result = await streamEvents("/api/apps/gitea", {}, () => {});
    expect(result.dropped).toBe(true);
  });
});
