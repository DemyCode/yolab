
let baseUrl = "";
let authToken: string | null = null;

export function configureApi(opts: {
  baseUrl?: string;
  token?: string | null;
}) {
  if (opts.baseUrl !== undefined) baseUrl = opts.baseUrl.replace(/\/$/, "");
  if (opts.token !== undefined) authToken = opts.token;
}

export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
  get isUnauthorized() {
    return this.status === 401;
  }
}

let onUnauthorized: (() => void) | null = null;
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn;
}

function errorMessageFrom(body: string, status: number): string {
  let message = body || `Request failed (${status})`;
  try {
    const parsed: unknown = JSON.parse(body);
    if (parsed && typeof parsed === "object" && "error" in parsed) {
      message = String((parsed as { error: unknown }).error);
    }
  } catch {
  }
  return message;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers);
  if (authToken) headers.set("Authorization", `Bearer ${authToken}`);
  if (init?.body && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }

  const res = await fetch(`${baseUrl}${path}`, {
    ...init,
    headers,
    credentials: "include",
  });

  if (res.status === 401) {
    onUnauthorized?.();
    throw new ApiError(401, "Your session expired. Please sign in again.");
  }
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new ApiError(res.status, errorMessageFrom(body, res.status));
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const api = {
  get: <T>(path: string) => request<T>(path),
  post: <T>(path: string, body?: unknown) =>
    request<T>(path, {
      method: "POST",
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
  put: <T>(path: string, body?: unknown) =>
    request<T>(path, {
      method: "PUT",
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
  del: <T>(path: string) => request<T>(path, { method: "DELETE" }),
};

export async function streamEvents(
  path: string,
  init: RequestInit,
  onLine: (line: string) => void,
): Promise<{ ok: boolean; error?: string }> {
  const headers = new Headers(init.headers);
  if (authToken) headers.set("Authorization", `Bearer ${authToken}`);
  if (init.body && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }

  const res = await fetch(`${baseUrl}${path}`, {
    ...init,
    headers,
    credentials: "include",
  });
  if (res.status === 401) {
    onUnauthorized?.();
    return { ok: false, error: "Your session expired. Please sign in again." };
  }
  if (!res.ok || !res.body) {
    const body = await res.text().catch(() => "");
    return { ok: false, error: errorMessageFrom(body, res.status) };
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let outcome: { ok: boolean; error?: string } | null = null;

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const frames = buffer.split("\n\n");
    buffer = frames.pop() ?? "";
    for (const frame of frames) {
      const line = frame.startsWith("data: ") ? frame.slice(6) : frame;
      if (!line.trim()) continue;
      if (line.startsWith("[ERROR]")) {
        outcome = { ok: false, error: line.replace("[ERROR]", "").trim() };
      } else if (line.startsWith("[DONE]")) {
        outcome = { ok: true };
      }
      onLine(line);
    }
  }

  return outcome ?? { ok: false, error: "The connection closed unexpectedly." };
}


export type ListResult<T> =
  | { ok: true; data: T[] }
  | { ok: false; reason: "unreachable" | "unauthorized" };

export async function fetchList<T>(url: string): Promise<ListResult<T>> {
  try {
    const body = await api.get<unknown>(url);
    if (!Array.isArray(body)) return { ok: false, reason: "unreachable" };
    return { ok: true, data: body as T[] };
  } catch (e) {
    if (e instanceof ApiError && e.isUnauthorized) {
      return { ok: false, reason: "unauthorized" };
    }
    return { ok: false, reason: "unreachable" };
  }
}

export interface CacheMeta {
  state: "hit" | "miss" | "stale" | "fresh";
  ageMs: number;
  ttlMs: number;
}

export function isCached(meta: CacheMeta | null): boolean {
  return meta !== null && (meta.state === "hit" || meta.state === "stale");
}

function metaFromHeaders(res: Response): CacheMeta | null {
  const state = res.headers.get("x-yolab-cache");
  if (!state) return null;
  return {
    state: state as CacheMeta["state"],
    ageMs: Number(res.headers.get("x-yolab-cache-age-ms") ?? 0),
    ttlMs: Number(res.headers.get("x-yolab-cache-ttl-ms") ?? 0),
  };
}

export async function getProgressive<T>(
  path: string,
  onFrame?: (data: T, meta: CacheMeta | null) => void,
): Promise<T> {
  const headers = new Headers({ "x-yolab-progressive": "1" });
  if (authToken) headers.set("Authorization", `Bearer ${authToken}`);

  const res = await fetch(`${baseUrl}${path}`, {
    headers,
    credentials: "include",
  });

  if (res.status === 401) {
    onUnauthorized?.();
    throw new ApiError(401, "Your session expired. Please sign in again.");
  }
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new ApiError(res.status, errorMessageFrom(body, res.status));
  }

  const isNdjson = (res.headers.get("content-type") ?? "").includes(
    "application/x-ndjson",
  );
  if (!isNdjson || !res.body) {
    const data = (await res.json()) as T;
    onFrame?.(data, metaFromHeaders(res));
    return data;
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let last: T | undefined;
  let seen = false;
  let failure: ApiError | null = null;

  const handleLine = (line: string) => {
    if (!line.trim()) return;
    const frame = JSON.parse(line) as {
      cache: string;
      ageMs?: number;
      ttlMs?: number;
      status?: number;
      data?: T;
    };
    if (frame.cache === "error") {
      failure = new ApiError(
        frame.status ?? 500,
        "The server could not refresh this value.",
      );
      return;
    }
    last = frame.data as T;
    seen = true;
    onFrame?.(frame.data as T, {
      state: frame.cache as CacheMeta["state"],
      ageMs: frame.ageMs ?? 0,
      ttlMs: frame.ttlMs ?? 0,
    });
  };

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let nl = buffer.indexOf("\n");
    while (nl !== -1) {
      handleLine(buffer.slice(0, nl));
      buffer = buffer.slice(nl + 1);
      nl = buffer.indexOf("\n");
    }
  }
  handleLine(buffer);

  if (failure) throw failure;
  if (!seen) throw new ApiError(502, "The server sent no data.");
  return last as T;
}
