import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  api,
  reasonOf,
  setUnauthorizedHandler,
  streamEvents,
} from "./api";

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
    expect(result).toEqual({ ok: true });
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
    expect(result).toMatchObject({ ok: false, dropped: true });
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
    expect(result).toMatchObject({ ok: false, dropped: true });
  });

  it("treats a request that never got through as dropped instead of throwing", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("Failed to fetch");
      }),
    );
    const result = await streamEvents("/api/apps/gitea", {}, () => {});
    expect(result).toMatchObject({ ok: false, dropped: true });
  });

  it("says the server took the request before the first line arrives", async () => {
    const order: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => streaming(frames("Installing…", "[DONE]"))),
    );
    await streamEvents(
      "/api/apps/gitea",
      {},
      (line) => order.push(line),
      () => order.push("open"),
    );
    expect(order).toEqual(["open", "Installing…", "[DONE]"]);
  });

  it("does not say the server took a request it refused", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response('{"error":"no"}', { status: 400 })),
    );
    const opened = vi.fn();
    const result = await streamEvents("/api/apps/gitea", {}, () => {}, opened);
    expect(result).toEqual({ ok: false, error: "no" });
    expect(opened).not.toHaveBeenCalled();
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

function reply(status: number, body = "", init: { json?: unknown } = {}) {
  return {
    status,
    ok: status >= 200 && status < 300,
    body: null,
    text: async () => body,
    json: async () => init.json,
  };
}

function lastCall(fetchMock: ReturnType<typeof vi.fn>) {
  const [path, init] = fetchMock.mock.calls.at(-1) as [string, RequestInit];
  return { path, init, headers: new Headers(init.headers) };
}

describe("api requests", () => {
  afterEach(() => setUnauthorizedHandler(null));

  it("GETs with the session cookie and returns the parsed body", async () => {
    const fetchMock = vi.fn(async () => reply(200, "", { json: { a: 1 } }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(api.get("/api/x")).resolves.toEqual({ a: 1 });
    const { path, init, headers } = lastCall(fetchMock);
    expect(path).toBe("/api/x");
    expect(init.credentials).toBe("include");
    expect(init.method).toBeUndefined();
    expect(headers.has("Content-Type")).toBe(false);
  });

  it("sends a body as JSON and says so", async () => {
    const fetchMock = vi.fn(async () => reply(200, "", { json: "ok" }));
    vi.stubGlobal("fetch", fetchMock);
    await api.post("/api/x", { a: 1 });
    expect(lastCall(fetchMock).init.method).toBe("POST");
    expect(lastCall(fetchMock).init.body).toBe('{"a":1}');
    expect(lastCall(fetchMock).headers.get("Content-Type")).toBe(
      "application/json",
    );
    await api.put("/api/x", [1]);
    expect(lastCall(fetchMock).init.method).toBe("PUT");
    expect(lastCall(fetchMock).init.body).toBe("[1]");
  });

  it("sends no body at all when there is nothing to send", async () => {
    const fetchMock = vi.fn(async () => reply(204));
    vi.stubGlobal("fetch", fetchMock);
    await expect(api.post("/api/x")).resolves.toBeUndefined();
    expect(lastCall(fetchMock).init.body).toBeUndefined();
    expect(lastCall(fetchMock).headers.has("Content-Type")).toBe(false);
    await api.put("/api/x");
    expect(lastCall(fetchMock).init.body).toBeUndefined();
    await api.del("/api/x");
    expect(lastCall(fetchMock).init.method).toBe("DELETE");
  });

  it("an expired session calls the sign-in handler and fails as unauthorized", async () => {
    const onUnauthorized = vi.fn();
    setUnauthorizedHandler(onUnauthorized);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => reply(401)),
    );
    const err = await api.get("/api/x").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(401);
    expect((err as ApiError).isUnauthorized).toBe(true);
    expect((err as ApiError).name).toBe("ApiError");
    expect((err as ApiError).message).toBe(
      "Your session expired. Please sign in again.",
    );
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });

  it("an expired session without a handler still fails", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => reply(401)),
    );
    await expect(api.get("/api/x")).rejects.toMatchObject({ status: 401 });
  });

  it("a refusal carries the server's error, its text, or its status", async () => {
    const cases: [string, string][] = [
      ['{"error":"disk full"}', "disk full"],
      ['{"detail":"x"}', '{"detail":"x"}'],
      ["null", "null"],
      ["plain words", "plain words"],
      ["", "Request failed (500)"],
    ];
    for (const [body, message] of cases) {
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => reply(500, body)),
      );
      const err = (await api
        .get("/api/x")
        .catch((e: unknown) => e)) as ApiError;
      expect(err.message, body).toBe(message);
      expect(err.status).toBe(500);
      expect(err.isUnauthorized).toBe(false);
    }
  });

  it("a refusal whose body cannot be read still says what failed", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        status: 503,
        ok: false,
        text: () => Promise.reject(new Error("reset")),
      })),
    );
    await expect(api.get("/api/x")).rejects.toMatchObject({
      message: "Request failed (503)",
    });
  });

  it("keeps a content type the caller chose", async () => {
    const fetchMock = vi.fn(async () => reply(204));
    vi.stubGlobal("fetch", fetchMock);
    await api.upload("/api/x", new Blob(["x"]));
    expect(lastCall(fetchMock).headers.get("Content-Type")).toBe(
      "application/octet-stream",
    );
  });
});

describe("streamEvents refusals", () => {
  afterEach(() => setUnauthorizedHandler(null));

  it("an expired session calls the sign-in handler", async () => {
    const onUnauthorized = vi.fn();
    setUnauthorizedHandler(onUnauthorized);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => reply(401)),
    );
    await expect(streamEvents("/s", {}, () => {})).resolves.toEqual({
      ok: false,
      error: "Your session expired. Please sign in again.",
    });
    expect(onUnauthorized).toHaveBeenCalledOnce();

    setUnauthorizedHandler(null);
    await expect(streamEvents("/s", {}, () => {})).resolves.toMatchObject({
      ok: false,
    });
  });

  it("a refused stream reports the server's reason", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => reply(409, '{"error":"busy"}')),
    );
    await expect(streamEvents("/s", {}, () => {})).resolves.toEqual({
      ok: false,
      error: "busy",
    });
  });

  it("a stream with no body is a failure", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => reply(200, "")),
    );
    await expect(streamEvents("/s", {}, () => {})).resolves.toEqual({
      ok: false,
      error: "Request failed (200)",
    });
  });

  it("a refused stream whose body cannot be read says what failed", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({
        status: 502,
        ok: false,
        body: null,
        text: () => Promise.reject(new Error("reset")),
      })),
    );
    await expect(streamEvents("/s", {}, () => {})).resolves.toEqual({
      ok: false,
      error: "Request failed (502)",
    });
  });

  it("passes on lines without a data prefix and skips blank frames", async () => {
    const chunks = [
      new TextEncoder().encode("raw line\n\n   \n\ndata: [DONE]\n\n"),
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        streaming(async () => {
          const value = chunks.shift();
          return value ? { done: false, value } : { done: true };
        }),
      ),
    );
    const lines: string[] = [];
    const result = await streamEvents("/s", {}, (l) => lines.push(l));
    expect(lines).toEqual(["raw line", "[DONE]"]);
    expect(result).toEqual({ ok: true });
  });
});

describe("reasonOf", () => {
  it("gives the server's reason, or the caller's words for anything else", () => {
    expect(reasonOf(new ApiError(500, "disk full"), "x")).toBe("disk full");
    expect(reasonOf(new TypeError("Failed to fetch"), "No answer.")).toBe(
      "No answer.",
    );
  });
});
