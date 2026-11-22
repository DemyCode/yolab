import { afterEach, describe, expect, it, vi } from "vitest";
import { api, streamEvents } from "./api";

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

  it("reports the server's own failure as a failure, not as a dropped connection", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        streaming(frames("Installing…", "[ERROR] helm said no")),
      ),
    );
    const result = await streamEvents("/api/apps/gitea", {}, () => {});
    expect(result).toEqual({ ok: false, error: "helm said no" });
  });

  it("treats a stream that ends without a verdict as dropped, because the server keeps going", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => streaming(frames("Installing…"))),
    );
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
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => streaming(read)),
    );
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

describe("api.upload", () => {
  it("sends the file as raw bytes, not JSON", async () => {
    const fetch = vi.fn(
      async () => new Response(JSON.stringify({ app: { id: "x" } })),
    );
    vi.stubGlobal("fetch", fetch);
    const file = new Blob(["chart bytes"]);
    expect(await api.upload("/api/apps/custom/chart", file)).toEqual({
      app: { id: "x" },
    });
    const [, init] = fetch.mock.calls[0] as unknown as [string, RequestInit];
    expect(init.method).toBe("POST");
    expect(init.body).toBe(file);
    expect(new Headers(init.headers).get("Content-Type")).toBe(
      "application/octet-stream",
    );
  });

  it("turns the server's refusal into an error carrying its reason", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ error: "not a chart" }), {
            status: 400,
          }),
      ),
    );
    await expect(
      api.upload("/api/apps/custom/chart", new Blob(["x"])),
    ).rejects.toThrow("not a chart");
  });
});
