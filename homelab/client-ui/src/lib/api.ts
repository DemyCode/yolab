// The single place the UI talks to local-api.
//
// Every request goes through `request()`. That matters for more than tidiness:
// the web build is served by local-api itself, so it can rely on same-origin
// requests and a session cookie — but the phone and desktop shells we want to
// derive from this codebase talk to a *remote* box over the tunnel, where
// neither of those holds. Keeping one chokepoint means those builds set a base
// URL and a bearer token here and nothing else in the app changes.

/** Where the API lives. Empty string = same origin, which is the web build. */
let baseUrl = "";
/** Bearer token, used only by builds that cannot rely on a session cookie. */
let authToken: string | null = null;

export function configureApi(opts: {
  baseUrl?: string;
  token?: string | null;
}) {
  if (opts.baseUrl !== undefined) baseUrl = opts.baseUrl.replace(/\/$/, "");
  if (opts.token !== undefined) authToken = opts.token;
}

export function getApiBaseUrl(): string {
  return baseUrl;
}

/** Thrown for any non-2xx response so callers can branch on `status`. */
export class ApiError extends Error {
  // Declared and assigned rather than a constructor parameter property: the
  // project builds with `erasableSyntaxOnly`, which rules out any TypeScript
  // that emits runtime code.
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
  /** The session expired — the caller should send the user back to sign in. */
  get isUnauthorized() {
    return this.status === 401;
  }
}

/** Set by App so a 401 anywhere drops straight back to the sign-in screen. */
let onUnauthorized: (() => void) | null = null;
export function setUnauthorizedHandler(fn: (() => void) | null) {
  onUnauthorized = fn;
}

async function request<T>(
  path: string,
  init?: RequestInit & { raw?: boolean },
): Promise<T> {
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
    // local-api returns errors as plain text on some routes and {"error": …}
    // on others; surface whichever we got rather than a bare status code.
    const body = await res.text().catch(() => "");
    let message = body || `Request failed (${res.status})`;
    try {
      const parsed: unknown = JSON.parse(body);
      if (parsed && typeof parsed === "object" && "error" in parsed) {
        message = String((parsed as { error: unknown }).error);
      }
    } catch {
      /* body was not JSON; the raw text is the better message anyway */
    }
    throw new ApiError(res.status, message);
  }

  if (init?.raw) return (await res.text()) as T;
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const api = {
  get: <T>(path: string) => request<T>(path),
  getText: (path: string) => request<string>(path, { raw: true }),
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

/**
 * Consume a Server-Sent Events route as a series of lines.
 *
 * Install and update stream Helm's output rather than returning JSON, and they
 * mark the end themselves: a line beginning `[DONE]` on success, `[ERROR]` on
 * failure. The stream closing is *not* a success signal — a helm process killed
 * mid-flight closes it too — so the caller is told which terminator it got.
 */
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
    return { ok: false, error: body || `Request failed (${res.status})` };
  }

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let outcome: { ok: boolean; error?: string } | null = null;

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    // SSE frames are separated by a blank line; a frame can arrive split
    // across reads, so only whole frames are consumed and the rest kept.
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

// ── Legacy list helper ──────────────────────────────────────────────────────
// Kept because the ported operator pages depend on the distinction it encodes:
// "the cluster said nothing is there" is a normal state (a fresh box has an
// empty app list) and must be allowed to overwrite a cached copy, whereas "we
// could not ask the cluster" must not. Collapsing the two used to leave an
// uninstalled app on screen forever behind a false "unreachable" banner.

export type ListResult<T> =
  | { ok: true; data: T[] }
  | { ok: false; reason: "unreachable" | "unauthorized" };

export async function fetchList<T>(url: string): Promise<ListResult<T>> {
  try {
    const body = await api.get<unknown>(url);
    // Error responses are JSON too, and `.length` on those is `undefined`
    // rather than throwing — so without this check a 200-shaped error body
    // would sail through and read as an empty list.
    if (!Array.isArray(body)) return { ok: false, reason: "unreachable" };
    return { ok: true, data: body as T[] };
  } catch (e) {
    if (e instanceof ApiError && e.isUnauthorized) {
      return { ok: false, reason: "unauthorized" };
    }
    return { ok: false, reason: "unreachable" };
  }
}
