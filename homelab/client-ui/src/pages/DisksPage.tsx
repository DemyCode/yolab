import { useEffect, useMemo, useRef, useState } from "react";
import { HardDrive, Plus, X, AlertTriangle, RefreshCw } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import type { CephStatus, DiskItem, DiskStatus } from "@/types/disk";

// ── Formatters ────────────────────────────────────────────────────────────────

function fmt(bytes: number | null | undefined): string {
  if (!bytes) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(1)} ${units[i]}`;
}

// ── Status palette ────────────────────────────────────────────────────────────

const STATUS: Record<DiskStatus, { bg: string; color: string; label: string }> = {
  unused:   { bg: "#1e1e24", color: "#52525b", label: "Unused"         },
  joining:  { bg: "#2d2a1a", color: "#fbbf24", label: "Setting up…"    },
  active:   { bg: "#1a2e1a", color: "#4ade80", label: "Active"         },
  removing: { bg: "#2d1e0a", color: "#fb923c", label: "Removing…"      },
  missing:  { bg: "#2d1a1a", color: "#f87171", label: "Disconnected"   },
};

// ── Skeletons ─────────────────────────────────────────────────────────────────

function Shimmer({ className }: { className?: string }) {
  return <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />;
}

function DiskRowSkeleton() {
  return (
    <Card><CardContent className="pt-5 pb-5">
      <div className="flex items-start gap-3">
        <Shimmer className="mt-0.5 h-7 w-7 rounded-md flex-shrink-0" />
        <div className="flex-1 space-y-2.5">
          <Shimmer className="h-3.5 w-40" />
          <div className="flex gap-2">
            <Shimmer className="h-5 w-20 rounded-full" />
            <Shimmer className="h-5 w-16 rounded-full" />
          </div>
        </div>
      </div>
    </CardContent></Card>
  );
}

// ── Storage overview ──────────────────────────────────────────────────────────

function StorageOverview({ status }: { status: CephStatus | null }) {
  if (!status?.available || !status.total_bytes) return null;
  const { used_bytes: used, total_bytes: total } = status;
  const pct = Math.round((used / total) * 100);
  const color = pct > 85 ? "#f87171" : pct > 65 ? "#fbbf24" : "#a78bfa";
  return (
    <Card><CardContent className="pt-4 pb-4">
      <div className="space-y-2">
        <div className="flex justify-between text-xs">
          <span className="text-[#fafafa] font-medium">{fmt(used)} used</span>
          <span className="text-[#71717a]">{fmt(total - used)} free · {pct}%</span>
        </div>
        <div className="h-2 rounded-full bg-[#27272a] overflow-hidden">
          <div className="h-full rounded-full transition-all" style={{ width: `${pct}%`, background: color }} />
        </div>
      </div>
    </CardContent></Card>
  );
}

// ── Disk row ──────────────────────────────────────────────────────────────────

type DiskItemExt = DiskItem & { offline?: boolean };

function DiskRow({
  disk, busy,
  onAdd, onRemove, onDismiss,
}: {
  disk: DiskItemExt;
  busy: boolean;
  onAdd: (d: DiskItem) => void;
  onRemove: (d: DiskItem) => void;
  onDismiss: (d: DiskItem) => void;
}) {
  const effectiveStatus: DiskStatus = disk.offline ? "missing" : disk.status;
  const s = STATUS[effectiveStatus];

  const usedPct = disk.used_bytes && disk.size_bytes
    ? Math.round((disk.used_bytes / disk.size_bytes) * 100) : null;
  const barColor = (usedPct ?? 0) > 85 ? "#f87171" : (usedPct ?? 0) > 65 ? "#fbbf24" : "#a78bfa";

  // ── Hint (pure info, no button references) ─────────────────────────────────
  let hint: string | null = null;
  if (disk.offline) {
    hint = "Machine offline — showing last known state";
  } else if (disk.status === "joining") {
    hint = "Setting up — usually ready in a few minutes";
  } else if (disk.status === "removing") {
    hint = "Moving data to other disks…";
  } else if (disk.status === "missing") {
    if (disk.safe_to_destroy === true)
      hint = "Data is safely stored elsewhere";
    else if (disk.safe_to_destroy === false)
      hint = "Replug to recover your data — or restore from backup before dismissing";
    else
      hint = "Cannot reach storage cluster — reconnect before taking action";
  }

  // ── Action button ──────────────────────────────────────────────────────────
  let action: React.ReactNode = null;
  if (!busy && !disk.offline) {
    if (disk.status === "unused") {
      action = (
        <Btn variant="primary" onClick={() => onAdd(disk)}>
          Use for storage
        </Btn>
      );
    } else if (disk.status === "active" && !disk.is_builtin) {
      if (disk.last_disk) {
        action = (
          <span className="text-xs text-[#52525b]" title="Removing the last disk would destroy all data">
            Only disk — cannot remove
          </span>
        );
      } else {
        action = (
          <Btn variant="ghost" onClick={() => onRemove(disk)}>
            Remove
          </Btn>
        );
      }
    } else if (disk.status === "removing") {
      action = (
        <Btn variant="primary" onClick={() => onAdd(disk)}>
          Keep disk
        </Btn>
      );
    } else if (disk.status === "missing" && !disk.is_builtin) {
      if (disk.safe_to_destroy === true) {
        action = (
          <Btn variant="danger" onClick={() => onDismiss(disk)}>
            Remove from list
          </Btn>
        );
      }
      // safe_to_destroy=false or null → no action, hint explains why
    }
  }

  const statusLabel = disk.offline ? "Offline"
    : disk.status === "active" && disk.is_builtin ? "Active · built-in"
    : s.label;

  return (
    <Card>
      <CardContent className="pt-4 pb-4">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0" style={{ background: s.bg }}>
            <HardDrive className="h-4 w-4" style={{ color: s.color }} strokeWidth={1.75} />
          </div>

          <div className="flex-1 min-w-0 space-y-2">
            {/* Name */}
            <div className="flex items-baseline gap-2">
              <span className="font-medium text-[#fafafa] text-sm leading-tight">
                {disk.model || disk.name}
              </span>
              {disk.model && (
                <span className="text-xs text-[#52525b] font-mono">{disk.name}</span>
              )}
            </div>

            {/* Status + size + hostname */}
            <div className="flex items-center gap-2 flex-wrap">
              <span
                className="rounded-full px-2 py-0.5 text-xs font-medium"
                style={{ background: s.bg, color: s.color }}
              >
                {statusLabel}
              </span>
              <span className="text-xs text-[#52525b]">{fmt(disk.size_bytes)}</span>
              <span className="text-xs text-[#3f3f46]">·</span>
              <span className="text-xs text-[#52525b]">{disk.hostname}</span>
            </div>

            {/* Usage bar (active only) */}
            {disk.status === "active" && usedPct !== null && (
              <div className="space-y-1">
                <div className="h-1.5 rounded-full bg-[#27272a] overflow-hidden">
                  <div className="h-full rounded-full transition-all"
                    style={{ width: `${usedPct}%`, background: barColor }} />
                </div>
                <span className="text-xs text-[#52525b]">
                  {fmt(disk.used_bytes)} used · {usedPct}%
                </span>
              </div>
            )}

            {/* Migration progress bar (removing only) */}
            {disk.status === "removing" && disk.migration_pct !== null && (
              <div className="space-y-1">
                <div className="h-1.5 rounded-full bg-[#27272a] overflow-hidden">
                  <div className="h-full rounded-full transition-all"
                    style={{ width: `${disk.migration_pct}%`, background: "#fb923c" }} />
                </div>
                <span className="text-xs text-[#71717a]">
                  {disk.migration_pct}% migrated
                </span>
              </div>
            )}

            {/* Hint */}
            {hint && <p className="text-xs text-[#71717a]">{hint}</p>}

            {/* Action */}
            {(action || busy) && (
              <div className="pt-1">
                {busy
                  ? <span className="text-xs text-[#52525b]">Working…</span>
                  : action}
              </div>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Button primitive ──────────────────────────────────────────────────────────

function Btn({ onClick, variant = "ghost", children }: {
  onClick: () => void;
  variant?: "primary" | "ghost" | "danger";
  children: React.ReactNode;
}) {
  const cls = {
    primary: "border-[#4ade80]/50 text-[#4ade80] hover:border-[#4ade80] hover:bg-[#4ade80]/5",
    ghost:   "border-[#3f3f46] text-[#a1a1aa] hover:border-[#52525b] hover:text-[#d4d4d8]",
    danger:  "border-[#f87171]/40 text-[#f87171] hover:border-[#f87171]/70 hover:bg-[#f87171]/5",
  }[variant];
  return (
    <button onClick={onClick}
      className={`rounded border px-3 py-1 text-xs transition-colors ${cls}`}>
      {children}
    </button>
  );
}

// ── Virtual disk form ─────────────────────────────────────────────────────────

const BOX_TYPES = [
  { id: "bx11", label: "1 TB",  price: "€6.90"  },
  { id: "bx21", label: "5 TB",  price: "€21.60" },
  { id: "bx31", label: "10 TB", price: "€37.30" },
  { id: "bx41", label: "20 TB", price: "€65.94" },
] as const;

function AddVirtualDiskForm({
  nodes, onDone, onCancel,
}: { nodes: { host: string; hostname: string }[]; onDone: () => void; onCancel: () => void }) {
  const [boxType, setBoxType] = useState<string>("bx11");
  const [host, setHost] = useState(nodes[0]?.host ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleCreate() {
    setBusy(true); setError(null);
    try {
      const res = await fetch("/api/disks/virtual", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ box_type: boxType, host: host || undefined }),
      });
      const json = await res.json() as { ok?: boolean; error?: string };
      if (!res.ok || !json.ok) setError(json.error ?? `Error ${res.status}`);
      else onDone();
    } catch (e) {
      setError(`Network error: ${e instanceof Error ? e.message : String(e)}`);
    } finally { setBusy(false); }
  }

  return (
    <Card><CardContent className="pt-5 pb-5">
      <div className="space-y-4">
        <div className="flex items-start justify-between">
          <p className="text-sm font-medium text-[#fafafa]">New virtual disk</p>
          <button onClick={onCancel} className="p-0.5 rounded hover:bg-[#27272a] transition-colors">
            <X className="h-4 w-4 text-[#52525b]" />
          </button>
        </div>

        <div className="grid grid-cols-4 gap-2">
          {BOX_TYPES.map((t) => (
            <button key={t.id} onClick={() => setBoxType(t.id)}
              className={`rounded-md border px-3 py-2 text-sm text-center transition-colors ${
                boxType === t.id
                  ? "border-[#a78bfa] bg-[#a78bfa]/10 text-[#fafafa]"
                  : "border-[#27272a] bg-[#18181b] text-[#71717a] hover:border-[#52525b]"
              }`}>
              <div className="font-medium">{t.label}</div>
              <div className="text-xs opacity-60">{t.price}/mo</div>
            </button>
          ))}
        </div>

        {nodes.length > 1 && (
          <select value={host} onChange={(e) => setHost(e.target.value)}
            className="w-full rounded-md bg-[#18181b] border border-[#27272a] px-3 py-1.5 text-sm text-[#fafafa] focus:outline-none focus:ring-1 focus:ring-[#a78bfa]">
            {nodes.map((n) => <option key={n.host} value={n.host}>{n.hostname}</option>)}
          </select>
        )}

        <Button onClick={handleCreate} disabled={busy}
          className="w-full bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium text-sm h-9 px-4">
          {busy ? "Creating…" : "Create"}
        </Button>
        <p className="text-xs text-[#52525b]">Encrypted at rest. Billed monthly.</p>
        {error && <p className="text-xs text-[#f87171]">{error}</p>}
      </div>
    </CardContent></Card>
  );
}

// ── DisksPage ─────────────────────────────────────────────────────────────────

const CACHE_KEY = "yolab:disks";
const INTERVAL_KEY = "yolab:disks:interval";
const AUTO_KEY = "yolab:disks:auto";

const INTERVALS = [
  { label: "5s", value: 5 }, { label: "10s", value: 10 },
  { label: "30s", value: 30 }, { label: "1m", value: 60 }, { label: "5m", value: 300 },
];

export function DisksPage() {
  const [disks, setDisks] = useState<DiskItemExt[]>([]);
  const [ceph, setCeph] = useState<CephStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [stale, setStale] = useState(false);
  const [addingVirtual, setAddingVirtual] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [tick, setTick] = useState(0);
  const [autoRefresh, setAutoRefresh] = useState(() => {
    try { return localStorage.getItem(AUTO_KEY) === "true"; } catch { return false; }
  });
  const [interval, setInterval_] = useState(() => {
    try { return parseInt(localStorage.getItem(INTERVAL_KEY) ?? "10", 10); } catch { return 10; }
  });
  const firstRef = useRef(true);

  // Auto-poll when disks are in transitional states, regardless of user setting.
  const needsFastPoll = disks.some(
    (d) => !d.offline && (d.status === "joining" || d.status === "removing")
  );

  useEffect(() => {
    try {
      localStorage.setItem(AUTO_KEY, String(autoRefresh));
      localStorage.setItem(INTERVAL_KEY, String(interval));
    } catch {}
  }, [autoRefresh, interval]);

  useEffect(() => {
    if (firstRef.current) {
      try {
        const cached = localStorage.getItem(CACHE_KEY);
        if (cached) setDisks(JSON.parse(cached) as DiskItemExt[]);
      } catch {}
    }

    function load() {
      if (busy) return;
      const p1 = fetch("/api/disks")
        .then((r) => r.json())
        .then((d: DiskItem[]) => {
          if (d.length > 0) {
            localStorage.setItem(CACHE_KEY, JSON.stringify(d));
            setDisks((prev) => {
              const freshHosts = new Set(d.map((x) => x.host));
              const offline = prev
                .filter((p) => !freshHosts.has(p.host))
                .map((p) => ({ ...p, offline: true }));
              const fresh = d.map((disk) => {
                const old = prev.find((p) => p.host === disk.host && p.name === disk.name);
                return {
                  ...disk,
                  used_bytes: disk.used_bytes ?? old?.used_bytes ?? null,
                  offline: false,
                };
              });
              setStale(offline.length > 0);
              return [...fresh, ...offline];
            });
          } else {
            setDisks((prev) => {
              if (prev.length > 0) { setStale(true); return prev.map((d) => ({ ...d, offline: true })); }
              return prev;
            });
          }
        })
        .catch(() => {
          setDisks((prev) => prev.map((d) => ({ ...d, offline: true })));
          setStale(true);
        });

      const p2 = fetch("/api/ceph/status")
        .then((r) => r.json())
        .then((d: CephStatus) => { if (d?.total_bytes > 0) setCeph(d); })
        .catch(() => {});

      if (firstRef.current) {
        firstRef.current = false;
        void Promise.all([p1, p2]).finally(() => { setLoading(false); setRefreshing(false); });
      } else {
        void Promise.all([p1, p2]).finally(() => setRefreshing(false));
      }
    }

    load();
    const id = (autoRefresh ? setInterval(load, interval * 1000) : needsFastPoll ? setInterval(load, 5000) : null);
    return () => { if (id) clearInterval(id); };
  }, [busy, tick, autoRefresh, interval, needsFastPoll]);

  const nodes = useMemo(() => {
    const seen = new Map<string, string>();
    disks.forEach((d) => seen.set(d.host, d.hostname));
    return Array.from(seen.entries()).map(([host, hostname]) => ({ host, hostname }));
  }, [disks]);

  async function post(path: string, disk: DiskItem, extra?: object) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    setError(null);
    try {
      const res = await fetch(path, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ disk_name: disk.name, host: disk.host, ...extra }),
      });
      const json = await res.json() as { ok?: boolean; reason?: string };
      if (!json.ok && json.reason) setError(json.reason);
    } catch (e) {
      setError(`Network error: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="space-y-6 max-w-3xl">
      {/* Header */}
      <div className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
          <p className="text-sm text-[#71717a] mt-0.5">
            Plug in a disk and click "Use for storage" to expand your storage pool.
          </p>
        </div>

        <div className="flex items-center gap-3 flex-shrink-0 pt-0.5">
          <button onClick={() => { setRefreshing(true); setTick((t) => t + 1); }}
            disabled={refreshing}
            className="flex items-center gap-1.5 text-xs text-[#a78bfa] hover:text-[#9061f9] transition-colors disabled:opacity-40">
            <RefreshCw className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`} />
            Refresh
          </button>

          <label className="flex items-center gap-1.5 cursor-pointer select-none">
            <input type="checkbox" checked={autoRefresh}
              onChange={(e) => setAutoRefresh(e.target.checked)}
              className="h-3.5 w-3.5 rounded border-[#3f3f46] bg-[#18181b] accent-[#a78bfa] cursor-pointer" />
            <span className="text-xs text-[#71717a]">Auto</span>
          </label>

          {autoRefresh && (
            <select value={interval} onChange={(e) => setInterval_(Number(e.target.value))}
              className="rounded bg-[#18181b] border border-[#27272a] px-2 py-0.5 text-xs text-[#a1a1aa] focus:outline-none focus:ring-1 focus:ring-[#a78bfa]">
              {INTERVALS.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
            </select>
          )}
        </div>
      </div>

      {/* Banners */}
      {stale && (
        <div className="flex items-start gap-2.5 rounded-lg border border-[#fbbf24]/30 bg-[#fbbf24]/5 px-4 py-3">
          <AlertTriangle className="h-4 w-4 text-[#fbbf24] mt-0.5 flex-shrink-0" />
          <p className="text-sm text-[#fbbf24]">One or more machines are offline — showing last known state.</p>
        </div>
      )}
      {error && (
        <div className="flex items-start gap-2.5 rounded-lg border border-[#f87171]/30 bg-[#f87171]/5 px-4 py-3">
          <AlertTriangle className="h-4 w-4 text-[#f87171] mt-0.5 flex-shrink-0" />
          <p className="text-sm text-[#f87171] flex-1">{error}</p>
          <button onClick={() => setError(null)} className="text-[#f87171]/60 hover:text-[#f87171]">
            <X className="h-4 w-4" />
          </button>
        </div>
      )}

      {/* Storage overview */}
      {!loading && <StorageOverview status={ceph} />}

      {/* Disk list */}
      {loading ? (
        <div className="space-y-3">{[1, 2, 3].map((i) => <DiskRowSkeleton key={i} />)}</div>
      ) : disks.length === 0 ? (
        <Card><CardContent className="py-12 flex flex-col items-center gap-4 text-center">
          <div className="rounded-full bg-[#27272a] p-4">
            <HardDrive className="h-7 w-7 text-[#52525b]" strokeWidth={1.5} />
          </div>
          <div>
            <p className="text-sm font-medium text-[#71717a]">No storage disks detected</p>
            <p className="text-xs text-[#52525b] mt-1">Disks appear here once the node is reachable</p>
          </div>
        </CardContent></Card>
      ) : (
        <div>
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">Disks</h2>
            {!addingVirtual && (
              <button onClick={() => setAddingVirtual(true)}
                className="flex items-center gap-1 text-xs text-[#a78bfa] hover:text-[#9061f9] transition-colors">
                <Plus className="h-3.5 w-3.5" />
                Add virtual disk
              </button>
            )}
          </div>

          {addingVirtual && (
            <div className="mb-3">
              <AddVirtualDiskForm nodes={nodes}
                onDone={() => setAddingVirtual(false)}
                onCancel={() => setAddingVirtual(false)} />
            </div>
          )}

          <div className="space-y-3">
            {disks.map((disk) => {
              const key = `${disk.host}:${disk.name}`;
              return (
                <DiskRow key={key} disk={disk} busy={busy === key}
                  onAdd={(d) => post("/api/disks/add", d)}
                  onRemove={(d) => post("/api/disks/remove", d)}
                  onDismiss={(d) => post("/api/disks/dismiss", d)} />
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}
