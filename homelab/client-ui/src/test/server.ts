import { vi } from "vitest";

export interface Call {
  method: string;
  url: string;
  body: unknown;
}

type Reply = unknown | ((call: Call) => unknown);

export function reply(status: number, body: unknown = "") {
  return () =>
    new Response(typeof body === "string" ? body : JSON.stringify(body), {
      status,
    });
}

export function stream(...lines: string[]) {
  return () =>
    new Response(lines.map((l) => `data: ${l}\n\n`).join(""), { status: 200 });
}

export function never() {
  return () => new Promise<Response>(() => {});
}

function bodyOf(init: RequestInit): unknown {
  if (typeof init.body !== "string") return init.body;
  try {
    return JSON.parse(init.body);
  } catch {
    return init.body;
  }
}

export function fakeServer(routes: Record<string, Reply> = {}) {
  const calls: Call[] = [];
  const fetchMock = vi.fn(async (url: string, init: RequestInit = {}) => {
    const call = { method: init.method ?? "GET", url, body: bodyOf(init) };
    calls.push(call);
    const key = `${call.method} ${url}`;
    if (!(key in routes)) {
      throw new Error(`no reply scripted for ${key}`);
    }
    const route = routes[key];
    const out = typeof route === "function" ? await route(call) : route;
    if (out instanceof Response) return out;
    if (out === undefined) return new Response(null, { status: 204 });
    return new Response(JSON.stringify(out), { status: 200 });
  });
  vi.stubGlobal("fetch", fetchMock);
  return {
    calls,
    routes,
    sent: (method: string, url: string) =>
      calls.filter((c) => c.method === method && c.url === url),
  };
}
