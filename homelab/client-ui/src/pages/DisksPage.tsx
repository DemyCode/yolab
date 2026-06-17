import { useEffect, useMemo, useState } from "react";
import { HardDrive, Plus, X } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import type { CephStatus, DiskItem, DrainRequest } from "@/types/disk";


function fmt(bytes: number | null | undefined): string {
  if (!bytes) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes,
    i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

function Shimmer({ className }: { className?: string }) {
  return (
    <div className={`animate-pulse rounded bg-[#27272a] ${className ?? ""}`} />
  );
}

function StorageOverviewSkeleton() {
  return (
    <Card>
      <CardContent className="pt-4 pb-4">
        <div className="space-y-2">
          <div className="flex justify-between">
            <Shimmer className="h-3 w-24" />
            <Shimmer className="h-3 w-28" />
          </div>
          <Shimmer className="h-2 w-full" />
        </div>
      </CardContent>
    </Card>
  );
}

function DiskRowSkeleton() {
  return (
    <Card>
      <CardContent className="pt-5 pb-5">
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
      </CardContent>
    </Card>
  );
}

function StorageOverview({ status }: { status: CephStatus | null }) {
  if (!status?.available || !status.total_bytes) return null;
  const { used_bytes: used, total_bytes: total } = status;
  const pct = Math.round((used / total) * 100);
  const color = pct > 85 ? "#f87171" : pct > 65 ? "#fbbf24" : "#a78bfa";
  return (
    <Card>
      <CardContent className="pt-4 pb-4">
        <div className="space-y-2">
          <div className="flex justify-between text-xs">
            <span className="text-[#fafafa] font-medium">{fmt(used)} used</span>
            <span className="text-[#71717a]">
              {fmt(total - used)} free · {pct}%
            </span>
          </div>
          <div className="h-2 rounded-full bg-[#27272a] overflow-hidden">
            <div
              className="h-full rounded-full transition-all"
              style={{ width: `${pct}%`, background: color }}
            />
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function DiskRow({
  disk,
  onRemove,
  removing,
}: {
  disk: DiskItem;
  onRemove: (disk: DiskItem) => void;
  removing: boolean;
}) {
  const usedPct =
    disk.used_bytes && disk.size_bytes
      ? Math.round((disk.used_bytes / disk.size_bytes) * 100)
      : null;
  const barColor =
    (usedPct ?? 0) > 85
      ? "#f87171"
      : (usedPct ?? 0) > 65
        ? "#fbbf24"
        : "#a78bfa";

  const statusLabel = disk.is_osd ? (
    <span className="text-xs text-[#4ade80]">In storage</span>
  ) : disk.is_builtin ? (
    <span className="text-xs text-[#fbbf24]">Built-in · can&apos;t unplug</span>
  ) : (
    <span className="text-xs text-[#fbbf24]">Safe to unplug</span>
  );

  return (
    <Card>
      <CardContent className="pt-5">
        <div className="flex items-start gap-3">
          <div
            className="mt-0.5 rounded-md p-1.5 flex-shrink-0"
            style={{ background: disk.is_osd ? "#1a2e1a" : "#2d2a1a" }}
          >
            <HardDrive
              className="h-4 w-4"
              style={{ color: disk.is_osd ? "#4ade80" : "#fbbf24" }}
              strokeWidth={1.75}
            />
          </div>

          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <div className="flex items-baseline gap-2">
                <span className="font-medium text-[#fafafa] text-sm">
                  {disk.model || disk.name}
                </span>
                {disk.model && (
                  <span className="text-xs text-[#52525b] font-mono">
                    {disk.name}
                  </span>
                )}
              </div>
              <div className="flex items-center gap-2">
                {statusLabel}
                <span className="text-xs text-[#52525b]">·</span>
                <span className="text-xs text-[#71717a]">
                  {fmt(disk.size_bytes)}
                </span>
                {!disk.is_builtin && (
                  <button
                    onClick={() => onRemove(disk)}
                    disabled={removing}
                    className="ml-1 text-xs text-[#52525b] hover:text-[#f87171] transition-colors disabled:opacity-40"
                    title="Remove from storage"
                  >
                    {removing ? "Removing…" : "Remove"}
                  </button>
                )}
              </div>
            </div>

            <div className="text-xs text-[#52525b] mt-0.5">
              {disk.hostname}
            </div>

            {disk.is_osd && (
              <div className="mt-2 space-y-1">
                <div className="h-1.5 rounded-full bg-[#27272a] overflow-hidden">
                  <div
                    className="h-full rounded-full transition-all"
                    style={{
                      width: `${usedPct ?? 0}%`,
                      background: barColor,
                    }}
                  />
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

interface AddVirtualDiskFormProps {
  nodes: { host: string; hostname: string }[];
  onDone: () => void;
  onCancel: () => void;
}

const BOX_TYPES = [
  { id: "bx11", label: "1 TB",  price: "€6.90"  },
  { id: "bx21", label: "5 TB",  price: "€21.60" },
  { id: "bx31", label: "10 TB", price: "€37.30" },
  { id: "bx41", label: "20 TB", price: "€65.94" },
] as const;

function AddVirtualDiskForm({ nodes, onDone, onCancel }: AddVirtualDiskFormProps) {
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
    } catch {
      setError("Request failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5">
        <div className="space-y-4">
          <div className="flex items-start justify-between">
            <p className="text-sm font-medium text-[#fafafa]">New virtual disk</p>
            <button
              onClick={onCancel}
              className="p-0.5 rounded hover:bg-[#27272a] transition-colors"
            >
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
                <label className="text-xs text-[#71717a]">Attached node</label>
                <div className="group relative">
                  <div className="h-3.5 w-3.5 rounded-full border border-[#52525b] flex items-center justify-center text-[9px] text-[#52525b] cursor-default leading-none">?</div>
                  <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 hidden group-hover:block z-10 w-48 rounded-md bg-[#27272a] border border-[#3f3f46] px-2.5 py-1.5 text-xs text-[#a1a1aa]">
                    The node that will mount and expose this disk to your apps.
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
            Provisions a dedicated virtual disk. Billed monthly, accessible over SFTP and mountable on any node.
          </p>

          {error && (
            <p className="text-xs text-[#f87171]">{error}</p>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

// ── DisksPage ─────────────────────────────────────────────────────────────────

export function DisksPage() {
  const [disks, setDisks] = useState<DiskItem[]>([]);
  const [ceph, setCeph] = useState<CephStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [addingVirtual, setAddingVirtual] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);

  useEffect(() => {
    let first = true;

    function load() {
      if (removing) return;

      const disksP = fetch("/api/disks")
        .then((r) => r.json())
        .then((d: DiskItem[]) => {
          if (d.length > 0) setDisks(prev => d.map(disk => {
            const old = prev.find(p => p.host === disk.host && p.name === disk.name);
            return {
              ...disk,
              used_bytes: disk.used_bytes ?? old?.used_bytes ?? null,
              free_bytes: disk.free_bytes ?? old?.free_bytes ?? null,
            };
          }));
        })
        .catch(() => {});

      const cephP = fetch("/api/ceph/status")
        .then((r) => r.json())
        .then((d: CephStatus) => { if (d?.total_bytes > 0) setCeph(d); })
        .catch(() => {});

      if (first) {
        first = false;
        void Promise.all([disksP, cephP]).finally(() => setLoading(false));
      }
    }

    load();
    const interval = setInterval(load, 10_000);
    return () => clearInterval(interval);
  }, [removing]);

  const nodes = useMemo(() => {
    const seen = new Map<string, string>();
    disks.forEach((d) => seen.set(d.host, d.hostname));
    return Array.from(seen.entries()).map(([host, hostname]) => ({ host, hostname }));
  }, [disks]);

  async function handleRemove(disk: DiskItem) {
    const key = `${disk.host}:${disk.name}`;
    setRemoving(key);
    try {
      const body: DrainRequest = { disk_name: disk.name, host: disk.host };
      await fetch("/api/disks/drain", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
    } finally {
      setRemoving(null);
    }
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          All disks are used automatically. Ceph distributes data across every OSD.
        </p>
      </div>

      {loading ? (
        <StorageOverviewSkeleton />
      ) : (
        <StorageOverview status={ceph} />
      )}

      {loading && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <Shimmer className="h-3 w-10" />
          </div>
          <div className="space-y-3">
            {[1, 2, 3].map((i) => (
              <DiskRowSkeleton key={i} />
            ))}
          </div>
        </div>
      )}

      {!loading && disks.length > 0 && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">
              Disks
            </h2>
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
                  onRemove={handleRemove}
                  removing={removing === key}
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
              <HardDrive
                className="h-7 w-7 text-[#52525b]"
                strokeWidth={1.5}
              />
            </div>
            <div>
              <p className="text-sm font-medium text-[#71717a]">
                No storage disks detected
              </p>
              <p className="text-xs text-[#52525b] mt-1">
                Disks appear here once the local API can reach the node
              </p>
            </div>
          </CardContent>
        </Card>
      )}

    </div>
  );
}
