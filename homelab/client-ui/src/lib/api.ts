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

/**
 * local-api returns errors as plain text on some routes and {"error": …} on
 * others; surface whichever we got rather than a bare status code. Shared by
 * `request` and `streamEvents` — the latter's SSE routes can also fail before
 * the stream even starts (e.g. the cluster-freeze middleware's 423 while a
 * restore is in flight), and that response is JSON like everything else.
 */
function errorMessageFrom(body: string, status: number): string {
  let message = body || `Request failed (${status})`;
  try {
    const parsed: unknown = JSON.parse(body);
    if (parsed && typeof parsed === "object" && "error" in parsed) {
      message = String((parsed as { error: unknown }).error);
    }
  } catch {
    /* body was not JSON; the raw text is the better message anyway */
  }
  return message;
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
    const body = await res.text().catch(() => "");
    throw new ApiError(res.status, errorMessageFrom(body, res.status));
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

/**
 * What the server says about the body it just gave us.
 *
 * `hit` and `miss` come from a single JSON response's headers; `stale` and
 * `fresh` are the two frames of a progressive one. The distinction the UI cares
 * about is only ever "is this the value the box just computed, or one it
 * remembered" — plus how old the remembered one is.
 */
export interface CacheMeta {
  state: "hit" | "miss" | "stale" | "fresh";
  ageMs: number;
  ttlMs: number;
}

/** True when the body was remembered rather than computed for this request. */
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

/**
 * Ask for the cached answer AND the real one, on a single request.
 *
 * The server replies with ndjson: frame one is whatever it already had, frame
 * two is what the handler actually produced. `onFrame` fires for each, so the
 * page can paint from the remembered value in milliseconds and correct itself a
 * few seconds later when the box has finished shelling out to `ceph`. Endpoints
 * measured between 0.9s and 5.5s on a healthy cluster, and unbounded on a sick
 * one, which is the whole reason this exists.
 *
 * Degrades in both directions. A route the server does not cache answers with
 * ordinary JSON and no cache headers, and this returns it with `meta` null — so
 * callers need no knowledge of which routes are cached. And a cold cache has no
 * first frame to send, so only one arrives.
 *
 * EVERY FRAME CARRIES ITS OWN META and the caller is expected to show it. A
 * remembered value rendered as though it were live is the exact bug that got
 * localStorage caching removed from `useResource`; this is only safe while the
 * UI keeps saying which one it is holding.
 */
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
  // Raised by an `error` frame, but only after the loop: the handler failing
  // does not invalidate frame one, which the page is already showing.
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
      // The status line was sent long before this, so the server could not use
      // it to report the failure — this frame is the only signal there is.
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
    // A frame is only complete at a newline; anything after the last one is a
    // partial line that the next chunk finishes.
    let nl = buffer.indexOf("\n");
    while (nl !== -1) {
      handleLine(buffer.slice(0, nl));
      buffer = buffer.slice(nl + 1);
      nl = buffer.indexOf("\n");
    }
  }
  handleLine(buffer);

  // Thrown even when frame one arrived: the caller already has that value via
  // `onFrame`, and useResource keeps showing it while marking the resource
  // stale — which is exactly the right outcome for "we showed you what we had
  // and the refresh behind it failed".
  if (failure) throw failure;
  if (!seen) throw new ApiError(502, "The server sent no data.");
  return last as T;
}
