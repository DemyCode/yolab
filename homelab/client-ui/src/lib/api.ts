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
    // eslint-disable-next-line no-empty
  } catch {}
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

async function requestText(path: string): Promise<string> {
  const headers = new Headers();
  if (authToken) headers.set("Authorization", `Bearer ${authToken}`);
  const res = await fetch(`${baseUrl}${path}`, {
    headers,
    credentials: "include",
  });
  if (res.status === 401) {
    onUnauthorized?.();
    throw new ApiError(401, "Your session expired. Please sign in again.");
  }
  const body = await res.text().catch(() => "");
  if (!res.ok) {
    throw new ApiError(res.status, errorMessageFrom(body, res.status));
  }
  return body;
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
  text: (path: string) => requestText(path),
};

export interface StreamResult {
  ok: boolean;
  error?: string;
  dropped?: boolean;
}

const DROPPED: StreamResult = {
  ok: false,
  dropped: true,
  error:
    "The connection to your server dropped. What it was doing keeps going there.",
};

export async function streamEvents(
  path: string,
  init: RequestInit,
  onLine: (line: string) => void,
): Promise<StreamResult> {
  const headers = new Headers(init.headers);
  if (authToken) headers.set("Authorization", `Bearer ${authToken}`);
  if (init.body && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }

  let res: Response;
  try {
    res = await fetch(`${baseUrl}${path}`, {
      ...init,
      headers,
      credentials: "include",
    });
  } catch {
    return DROPPED;
  }
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
  let outcome: StreamResult | null = null;

  for (;;) {
    let chunk: ReadableStreamReadResult<Uint8Array>;
    try {
      chunk = await reader.read();
    } catch {
      break;
    }
    const { done, value } = chunk;
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

  return outcome ?? DROPPED;
}
