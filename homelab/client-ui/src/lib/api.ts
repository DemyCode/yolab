// Cluster list fetches, with "the cluster said nothing is there" kept distinct from
// "we couldn't ask the cluster".
//
// Both pages that show cached cluster state used to collapse these two cases: any empty
// response was treated as "K3s may be unreachable", which meant uninstalling your last
// app produced a red "Cluster API unreachable" banner AND kept rendering the app you
// had just deleted, indefinitely, because the stale localStorage copy was never
// overwritten with the empty truth. An authoritative empty list is a normal state — a
// fresh install has one — and it has to be able to overwrite the cache.

export type ListResult<T> =
  | { ok: true; data: T[] }
  /// The request itself failed: network error, non-2xx, or a body that isn't a list.
  /// Callers should keep showing whatever they already have and flag it as stale.
  | { ok: false; reason: "unreachable" | "unauthorized" };

export async function fetchList<T>(url: string): Promise<ListResult<T>> {
  try {
    const r = await fetch(url);
    // A 401 is the session expiring, not the control plane being down — telling the
    // user their cluster is unreachable when they actually just need to log in again
    // sends them debugging the wrong thing entirely.
    if (r.status === 401) return { ok: false, reason: "unauthorized" };
    if (!r.ok) return { ok: false, reason: "unreachable" };
    const body: unknown = await r.json();
    // Error responses are JSON too ({"error": …}), and `.length` on those is undefined
    // rather than throwing — so without this check a 200-shaped error body would sail
    // through and read as an empty list.
    if (!Array.isArray(body)) return { ok: false, reason: "unreachable" };
    return { ok: true, data: body as T[] };
  } catch {
    return { ok: false, reason: "unreachable" };
  }
}
