import { useEffect, useMemo, useRef, useState } from "react";
import { HardDrive, Plus, X, AlertTriangle, RefreshCw } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import type { CephStatus, DiskItem, DiskStatus } from "@/types/disk";

function fmt(bytes: number | null | undefined): string {
  if (!bytes) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(1)} ${units[i]}`;
}

// ── Status palette ────────────────────────────────────────────────────────────

const STATUS_STYLE: Record<DiskStatus, {
  iconBg: string;
  iconColor: string;
  labelColor: string;
  label: string;
}> = {
  active:   { iconBg: "#1a2e1a", iconColor: "#4ade80", labelColor: "#4ade80", label: "Active"      },
  pending:  { iconBg: "#1e1e24", iconColor: "#71717a", labelColor: "#71717a", label: "Non Active"  },
  joining:  { iconBg: "#2d2a1a", iconColor: "#fbbf24", labelColor: "#fbbf24", label: "Joining…"    },
  draining: { iconBg: "#2d1e0a", iconColor: "#fb923c", labelColor: "#fb923c", label: "Draining…"  },
  missing:  { iconBg: "#2d1a1a", iconColor: "#f87171", labelColor: "#f87171", label: "Missing"     },
};

// ── Skeletons ─────────────────────────────────────────────────────────────────

function Shimmer({ className }: { className?: string }) {
  return <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />;
}

function StorageOverviewSkeleton() {
  return (
    <Card><CardContent className="pt-4 pb-4">
      <div className="space-y-2">
        <div className="flex justify-between">
          <Shimmer className="h-3 w-24" />
          <Shimmer className="h-3 w-28" />
        </div>
        <Shimmer className="h-2 w-full" />
      </div>
    </CardContent></Card>
  );
}

function DiskRowSkeleton() {
  return (
    <Card><CardContent className="pt-5 pb-5">
      <div className="flex items-start gap-3">
        <Shimmer className="mt-0.5 h-7 w-7 rounded-md flex-shrink-0" />
        <div className="flex-1 space-y-2.5">
          <div className="flex items-center justify-between gap-4">
            <Shimmer className="h-3.5 w-40" />
            <Shimmer className="h-3 w-24" />
          </div>
          <Shimmer className="h-3 w-20" />
        </div>
      </div>
    </CardContent></Card>
  );
}

// ── Storage overview bar ──────────────────────────────────────────────────────

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

function DiskRow({
  disk,
  onJoin,
  onDrain,
  onCancelDrain,
  onRemove,
  busy,
}: {
  disk: DiskItemExt;
  onJoin: (disk: DiskItem) => void;
  onDrain: (disk: DiskItem) => void;
  onCancelDrain: (disk: DiskItem) => void;
  onRemove: (disk: DiskItem, force: boolean) => void;
  busy: boolean;
}) {
  const usedPct =
    disk.used_bytes && disk.size_bytes
      ? Math.round((disk.used_bytes / disk.size_bytes) * 100)
      : null;
  const barColor =
    (usedPct ?? 0) > 85 ? "#f87171" : (usedPct ?? 0) > 65 ? "#fbbf24" : "#a78bfa";

  const effectiveStatus: DiskStatus = disk.offline ? "missing" : disk.status;
  const style = STATUS_STYLE[effectiveStatus];

  const statusLabel = disk.offline ? (
    <span className="text-xs" style={{ color: style.labelColor }}>Machine offline</span>
  ) : (
    <span className="text-xs" style={{ color: style.labelColor }}>
      {style.label}
      {disk.status === "active" && disk.is_builtin ? " · built-in" : ""}
    </span>
  );

  return (
    <Card>
      <CardContent className="pt-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0" style={{ background: style.iconBg }}>
            <HardDrive className="h-4 w-4" style={{ color: style.iconColor }} strokeWidth={1.75} />
          </div>

          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <div className="flex items-baseline gap-2">
                <span className="font-medium text-[#fafafa] text-sm">{disk.model || disk.name}</span>
                {disk.model && (
                  <span className="text-xs text-[#52525b] font-mono">{disk.name}</span>
                )}
              </div>

              <div className="flex items-center gap-2">
                {statusLabel}
                <span className="text-xs text-[#52525b]">·</span>
                <span className="text-xs text-[#71717a]">{fmt(disk.size_bytes)}</span>

                {/* Non Active → Active */}
                {disk.status === "pending" && !disk.offline && (
                  <button
                    onClick={() => onJoin(disk)}
                    disabled={busy}
                    className="ml-1 text-xs text-[#4ade80] hover:text-[#22c55e] transition-colors disabled:opacity-40"
                  >
                    {busy ? "Activating…" : "Set Active"}
                  </button>
                )}

                {/* Draining → Active (cancel drain) */}
                {disk.status === "draining" && !disk.offline && (
                  <button
                    onClick={() => onCancelDrain(disk)}
                    disabled={busy}
                    className="ml-1 text-xs text-[#4ade80] hover:text-[#22c55e] transition-colors disabled:opacity-40"
                    title="Cancel drain and bring disk back to Active"
                  >
                    {busy ? "Activating…" : "Set Active"}
                  </button>
                )}

                {/* Active or Joining → Non Active */}
                {(disk.status === "active" || disk.status === "joining") && !disk.offline && (
                  <button
                    onClick={() => onDrain(disk)}
                    disabled={busy}
                    className="ml-1 text-xs text-[#52525b] hover:text-[#71717a] transition-colors disabled:opacity-40"
                    title="Drain data off this disk and make it Non Active"
                  >
                    {busy ? "Deactivating…" : "Set Non Active"}
                  </button>
                )}

                {/* Missing → Non Active (remove from Ceph so it can be re-joined if replugged) */}
                {disk.status === "missing" && !disk.is_builtin && (
                  disk.safe_to_destroy === true ? (
                    <button
                      onClick={() => onRemove(disk, false)}
                      disabled={busy}
                      className="ml-1 text-xs text-[#71717a] hover:text-[#a1a1aa] transition-colors disabled:opacity-40"
                      title="Remove from Ceph — if replugged, disk will appear as Non Active"
                    >
                      {busy ? "Removing…" : "Set Non Active"}
                    </button>
                  ) : disk.safe_to_destroy === false ? (
                    <button
                      onClick={() => onRemove(disk, true)}
                      disabled={busy}
                      className="ml-1 text-xs text-[#52525b] hover:text-[#71717a] transition-colors disabled:opacity-40"
                      title="Data not replicated — restore from Velero after this"
                    >
                      {busy ? "Removing…" : "Force Non Active"}
                    </button>
                  ) : (
                    <span className="ml-1 text-xs text-[#52525b]" title="Cluster unreachable — cannot verify safety">
                      Cluster unreachable
                    </span>
                  )
                )}
              </div>
            </div>

            <div className="text-xs text-[#52525b] mt-0.5">{disk.hostname}</div>

            {/* Missing: recovery hint */}
            {disk.status === "missing" && !disk.offline && (
              <div className="mt-1 text-xs" style={{ color: disk.safe_to_destroy === true ? "#4ade80" : "#71717a" }}>
                {disk.safe_to_destroy === true
                  ? "Data is safe — replug to go Active again, or set Non Active to remove from Ceph"
                  : disk.safe_to_destroy === false
                  ? "Replug disk to recover data — or force Non Active and restore from backup"
                  : "Cannot verify cluster state — reconnect Ceph before taking action"}
              </div>
            )}

            {/* Usage bar — active disks */}
            {disk.status === "active" && (
              <div className="mt-2 space-y-1">
                <div className="h-1.5 rounded-full bg-[#27272a] overflow-hidden">
                  <div className="h-full rounded-full transition-all" style={{ width: `${usedPct ?? 0}%`, background: barColor }} />
                </div>
                <span className="text-xs text-[#71717a]">
                  {disk.used_bytes != null ? `${fmt(disk.used_bytes)} used · ${usedPct}%` : "—"}
                </span>
              </div>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ── Add virtual disk form ──────────────────────────────────────────────────────

const BOX_TYPES = [
  { id: "bx11", label: "1 TB",  price: "€6.90"  },
  { id: "bx21", label: "5 TB",  price: "€21.60" },
  { id: "bx31", label: "10 TB", price: "€37.30" },
  { id: "bx41", label: "20 TB", price: "€65.94" },
] as const;

function AddVirtualDiskForm({
  nodes,
  onDone,
  onCancel,
}: {
  nodes: { host: string; hostname: string }[];
  onDone: () => void;
  onCancel: () => void;
}) {
  const [boxType, setBoxType] = useState<string>("bx11");
  const [host, setHost] = useState(nodes[0]?.host ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleCreate() {
    setBusy(true);
    setError(null);
    try {
      const res = await fetch("/api/disks/virtual", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ box_type: boxType, host: host || undefined }),
      });
      const json = await res.json() as { ok?: boolean; error?: string };
      if (!res.ok || !json.ok) {
        setError(json.error ?? `Server error ${res.status}`);
      } else {
        onDone();
      }
    } catch (e) {
      setError(`Network error: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
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

        <div className="space-y-1.5">
          <label className="text-xs text-[#71717a]">Storage size</label>
          <div className="grid grid-cols-4 gap-2">
            {BOX_TYPES.map((t) => (
              <button
                key={t.id}
                onClick={() => setBoxType(t.id)}
                className={`rounded-md border px-3 py-2 text-sm text-center transition-colors ${
                  boxType === t.id
                    ? "border-[#a78bfa] bg-[#a78bfa]/10 text-[#fafafa]"
                    : "border-[#27272a] bg-[#18181b] text-[#71717a] hover:border-[#52525b]"
                }`}
              >
                <div className="font-medium">{t.label}</div>
                <div className="text-xs opacity-60">{t.price}/mo</div>
              </button>
            ))}
          </div>
        </div>

        {nodes.length > 1 && (
          <div className="space-y-1.5">
            <div className="flex items-center gap-1.5">
              <label className="text-xs text-[#71717a]">Attached machine</label>
              <div className="group relative">
                <div className="h-3.5 w-3.5 rounded-full border border-[#52525b] flex items-center justify-center text-[9px] text-[#52525b] cursor-default leading-none">?</div>
                <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 hidden group-hover:block z-10 w-56 rounded-md bg-[#27272a] border border-[#3f3f46] px-2.5 py-1.5 text-xs text-[#a1a1aa]">
                  If the machine goes offline, so does the virtual disk.
                </div>
              </div>
            </div>
            <select
              value={host}
              onChange={(e) => setHost(e.target.value)}
              className="w-full rounded-md bg-[#18181b] border border-[#27272a] px-3 py-1.5 text-sm text-[#fafafa] focus:outline-none focus:ring-1 focus:ring-[#a78bfa]"
            >
              {nodes.map((n) => (
                <option key={n.host} value={n.host}>{n.hostname}</option>
              ))}
            </select>
          </div>
        )}

        <Button
          onClick={handleCreate}
          disabled={busy}
          className="w-full bg-[#a78bfa] hover:bg-[#9061f9] text-[#09090b] font-medium text-sm h-9 px-4"
        >
          {busy ? "Creating…" : "Create"}
        </Button>

        <p className="text-xs text-[#52525b]">
          Encrypted at rest — only your key, only your data. Billed monthly.
        </p>

        {error && <p className="text-xs text-[#f87171]">{error}</p>}
      </div>
    </CardContent></Card>
  );
}

// ── DisksPage ─────────────────────────────────────────────────────────────────

const DISKS_CACHE_KEY = "yolab:disks";
const REFRESH_INTERVAL_KEY = "yolab:disks:refreshInterval";
const AUTO_REFRESH_KEY = "yolab:disks:autoRefresh";

const INTERVAL_OPTIONS = [
  { label: "5s",  value: 5   },
  { label: "10s", value: 10  },
  { label: "30s", value: 30  },
  { label: "1m",  value: 60  },
  { label: "5m",  value: 300 },
];

type DiskItemExt = DiskItem & { offline?: boolean };

export function DisksPage() {
  const [disks, setDisks] = useState<DiskItemExt[]>([]);
  const [ceph, setCeph] = useState<CephStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [addingVirtual, setAddingVirtual] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [stale, setStale] = useState(false);
  const [refreshTick, setRefreshTick] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const [autoRefresh, setAutoRefresh] = useState<boolean>(() => {
    try { return localStorage.getItem(AUTO_REFRESH_KEY) === "true"; } catch { return false; }
  });
  const [refreshInterval, setRefreshInterval] = useState<number>(() => {
    try { return parseInt(localStorage.getItem(REFRESH_INTERVAL_KEY) ?? "10", 10); } catch { return 10; }
  });
  const firstRef = useRef(true);

  // Persist refresh settings
  useEffect(() => {
    try {
      localStorage.setItem(AUTO_REFRESH_KEY, String(autoRefresh));
      localStorage.setItem(REFRESH_INTERVAL_KEY, String(refreshInterval));
    } catch {}
  }, [autoRefresh, refreshInterval]);

  useEffect(() => {
    // Pre-populate from cache on first mount
    if (firstRef.current) {
      try {
        const cached = localStorage.getItem(DISKS_CACHE_KEY);
        if (cached) setDisks(JSON.parse(cached) as DiskItemExt[]);
      } catch {}
    }

    function load() {
      if (busy) return;

      const disksP = fetch("/api/disks")
        .then((r) => r.json())
        .then((d: DiskItem[]) => {
          if (d.length > 0) {
            localStorage.setItem(DISKS_CACHE_KEY, JSON.stringify(d));
            setDisks((prev) => {
              const freshHosts = new Set(d.map((x) => x.host));
              const offlineDisks: DiskItemExt[] = prev
                .filter((p) => !freshHosts.has(p.host))
                .map((p) => ({ ...p, offline: true }));
              const freshMerged = d.map((disk) => {
                const old = prev.find((p) => p.host === disk.host && p.name === disk.name);
                return {
                  ...disk,
                  used_bytes: disk.used_bytes ?? old?.used_bytes ?? null,
                  free_bytes: disk.free_bytes ?? old?.free_bytes ?? null,
                  offline: false,
                };
              });
              setStale(offlineDisks.length > 0);
              return [...freshMerged, ...offlineDisks];
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

      const cephP = fetch("/api/ceph/status")
        .then((r) => r.json())
        .then((d: CephStatus) => { if (d?.total_bytes > 0) setCeph(d); })
        .catch(() => {});

      if (firstRef.current) {
        firstRef.current = false;
        void Promise.all([disksP, cephP]).finally(() => { setLoading(false); setRefreshing(false); });
      } else {
        void Promise.all([disksP, cephP]).finally(() => setRefreshing(false));
      }
    }

    load();

    const interval = autoRefresh ? setInterval(load, refreshInterval * 1000) : null;
    return () => { if (interval) clearInterval(interval); };
  }, [busy, refreshTick, autoRefresh, refreshInterval]);

  function handleManualRefresh() {
    setRefreshing(true);
    setRefreshTick((t) => t + 1);
  }

  const nodes = useMemo(() => {
    const seen = new Map<string, string>();
    disks.forEach((d) => seen.set(d.host, d.hostname));
    return Array.from(seen.entries()).map(([host, hostname]) => ({ host, hostname }));
  }, [disks]);

  async function handleJoin(disk: DiskItem) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    try {
      await fetch("/api/disks/join", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
      });
    } finally {
      setBusy(null);
    }
  }

  async function handleDrain(disk: DiskItem) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    try {
      await fetch("/api/disks/drain", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
      });
    } finally {
      setBusy(null);
    }
  }

  async function handleCancelDrain(disk: DiskItem) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    try {
      await fetch("/api/disks/cancel-drain", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
      });
    } finally {
      setBusy(null);
    }
  }

  async function handleRemove(disk: DiskItem, force: boolean) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    try {
      await fetch("/api/disks/remove", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ disk_name: disk.name, host: disk.host, force }),
      });
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
          <p className="text-sm text-[#71717a] mt-0.5">
            All disks are used automatically. Ceph distributes data across every OSD.
          </p>
        </div>

        {/* Refresh controls */}
        <div className="flex items-center gap-3 flex-shrink-0 pt-0.5">
          <button
            onClick={handleManualRefresh}
            disabled={refreshing}
            className="flex items-center gap-1.5 text-xs text-[#a78bfa] hover:text-[#9061f9] transition-colors disabled:opacity-40"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`} />
            Refresh
          </button>

          <label className="flex items-center gap-1.5 cursor-pointer select-none">
            <input
              type="checkbox"
              checked={autoRefresh}
              onChange={(e) => setAutoRefresh(e.target.checked)}
              className="h-3.5 w-3.5 rounded border-[#3f3f46] bg-[#18181b] accent-[#a78bfa] cursor-pointer"
            />
            <span className="text-xs text-[#71717a]">Auto</span>
          </label>

          {autoRefresh && (
            <select
              value={refreshInterval}
              onChange={(e) => setRefreshInterval(Number(e.target.value))}
              className="rounded bg-[#18181b] border border-[#27272a] px-2 py-0.5 text-xs text-[#a1a1aa] focus:outline-none focus:ring-1 focus:ring-[#a78bfa]"
            >
              {INTERVAL_OPTIONS.map((o) => (
                <option key={o.value} value={o.value}>{o.label}</option>
              ))}
            </select>
          )}
        </div>
      </div>

      {stale && (
        <div className="flex items-start gap-2.5 rounded-lg border border-[#fbbf24]/30 bg-[#fbbf24]/5 px-4 py-3">
          <AlertTriangle className="h-4 w-4 text-[#fbbf24] mt-0.5 flex-shrink-0" />
          <p className="text-sm text-[#fbbf24]">
            One or more machines are offline — their disks are shown as last known.
          </p>
        </div>
      )}

      {loading ? <StorageOverviewSkeleton /> : <StorageOverview status={ceph} />}

      {loading && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <Shimmer className="h-3 w-10" />
          </div>
          <div className="space-y-3">
            {[1, 2, 3].map((i) => <DiskRowSkeleton key={i} />)}
          </div>
        </div>
      )}

      {!loading && disks.length > 0 && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">Disks</h2>
            {!addingVirtual && (
              <button
                onClick={() => setAddingVirtual(true)}
                className="flex items-center gap-1 text-xs text-[#a78bfa] hover:text-[#9061f9] transition-colors"
              >
                <Plus className="h-3.5 w-3.5" />
                Add virtual disk
              </button>
            )}
          </div>

          {addingVirtual && (
            <div className="mb-3">
              <AddVirtualDiskForm
                nodes={nodes}
                onDone={() => setAddingVirtual(false)}
                onCancel={() => setAddingVirtual(false)}
              />
            </div>
          )}

          <div className="space-y-3">
            {disks.map((disk) => {
              const key = `${disk.host}:${disk.name}`;
              return (
                <DiskRow
                  key={key}
                  disk={disk}
                  onJoin={handleJoin}
                  onDrain={handleDrain}
                  onCancelDrain={handleCancelDrain}
                  onRemove={handleRemove}
                  busy={busy === key}
                />
              );
            })}
          </div>
        </div>
      )}

      {!loading && disks.length === 0 && (
        <Card>
          <CardContent className="py-12 flex flex-col items-center gap-4 text-center">
            <div className="rounded-full bg-[#27272a] p-4">
              <HardDrive className="h-7 w-7 text-[#52525b]" strokeWidth={1.5} />
            </div>
            <div>
              <p className="text-sm font-medium text-[#71717a]">No storage disks detected</p>
              <p className="text-xs text-[#52525b] mt-1">Disks appear here once the local API can reach the node</p>
            </div>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
