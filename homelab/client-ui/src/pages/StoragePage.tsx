import { useEffect, useState } from "react";
import { RefreshCw, HardDrive, AlertTriangle } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import type { OsdInfo, PoolInfo, StorageDetail, StorageDetailResponse } from "@/types/storage";

const GiB = 1073741824;
const TiB = GiB * 1024;

function fmtBytes(b: number): string {
  if (b >= TiB) return `${(b / TiB).toFixed(2)} TiB`;
  if (b >= GiB) return `${(b / GiB).toFixed(1)} GiB`;
  if (b >= 1048576) return `${(b / 1048576).toFixed(0)} MiB`;
  return `${(b / 1024).toFixed(0)} KiB`;
}

function fillColor(pct: number): string {
  if (pct >= 85) return "#f87171";
  if (pct >= 70) return "#fbbf24";
  return "#4ade80";
}

function FillBar({ pct }: { pct: number }) {
  const color = fillColor(pct);
  return (
    <div className="flex items-center gap-2 min-w-[120px]">
      <div className="flex-1 h-1.5 rounded-full bg-[#27272a] overflow-hidden">
        <div
          className="h-full rounded-full transition-all"
          style={{ width: `${Math.min(pct, 100)}%`, background: color }}
        />
      </div>
      <span className="text-xs tabular-nums w-10 text-right" style={{ color }}>
        {pct.toFixed(1)}%
      </span>
    </div>
  );
}

function VarBadge({ v }: { v: number }) {
  const ok = v >= 0.7 && v <= 1.4;
  const warn = !ok && v >= 0.4 && v <= 2.0;
  return (
    <span className={cn(
      "text-xs tabular-nums font-mono",
      ok   ? "text-[#71717a]" :
      warn ? "text-[#fbbf24]" : "text-[#f87171]",
    )}>
      {v.toFixed(2)}
    </span>
  );
}

function OsdTable({ osds }: { osds: OsdInfo[] }) {
  const hosts = [...new Set(osds.map(o => o.host))].sort();

  return (
    <Card>
      <CardHeader>
        <CardTitle>Disks</CardTitle>
      </CardHeader>
      <CardContent className="p-0">
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-[#27272a]">
                {["OSD", "Host", "Class", "Size", "Fill", "Used / Free", "PGs", "Balance", "Status"].map(h => (
                  <th key={h} className="px-4 py-2.5 text-left text-xs font-medium text-[#71717a] whitespace-nowrap first:pl-6 last:pr-6">{h}</th>
                ))}
              </tr>
            </thead>
            <tbody className="divide-y divide-[#27272a]/40">
              {hosts.flatMap(host => {
                const hostOsds = osds.filter(o => o.host === host);
                return hostOsds.map((osd, idx) => (
                  <tr key={osd.id} className="hover:bg-[#27272a]/20 transition-colors">
                    <td className="pl-6 pr-4 py-3 font-mono text-xs text-[#a78bfa]">{osd.name}</td>
                    <td className="px-4 py-3 text-xs text-[#71717a]">
                      {idx === 0 && (
                        <span className="inline-flex items-center gap-1">
                          <HardDrive className="h-3 w-3" />
                          {host}
                        </span>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      <Badge variant="muted" className="text-xs uppercase">
                        {osd.class || "—"}
                      </Badge>
                    </td>
                    <td className="px-4 py-3 text-xs text-[#a1a1aa] whitespace-nowrap tabular-nums">
                      {fmtBytes(osd.size_bytes)}
                    </td>
                    <td className="px-4 py-3">
                      <FillBar pct={osd.utilization} />
                    </td>
                    <td className="px-4 py-3 text-xs text-[#71717a] whitespace-nowrap tabular-nums">
                      {fmtBytes(osd.used_bytes)} / {fmtBytes(osd.avail_bytes)}
                    </td>
                    <td className="px-4 py-3 text-xs text-[#71717a] tabular-nums">{osd.pgs}</td>
                    <td className="px-4 py-3"><VarBadge v={osd.var} /></td>
                    <td className="pr-6 px-4 py-3">
                      <Badge variant={osd.status === "up" ? "success" : "destructive"}>
                        {osd.status}
                      </Badge>
                    </td>
                  </tr>
                ));
              })}
            </tbody>
          </table>
        </div>
      </CardContent>
    </Card>
  );
}

function estimateUsable(osds: OsdInfo[], size: number, domain: "osd" | "host"): number {
  const SAFETY = 0.95;
  const totalRaw = osds.reduce((s, o) => s + o.size_bytes, 0);

  if (domain === "osd") {
    if (osds.length <= size) {
      return Math.min(...osds.map(o => o.size_bytes)) * SAFETY;
    }
    return (totalRaw / size) * SAFETY;
  } else {
    const hostMap = new Map<string, number>();
    for (const o of osds) hostMap.set(o.host, (hostMap.get(o.host) ?? 0) + o.size_bytes);
    const caps = [...hostMap.values()];
    if (caps.length < size) return 0;
    if (caps.length === size) return Math.min(...caps) * SAFETY;
    return (totalRaw / size) * SAFETY;
  }
}

function domainLabel(d: string): string {
  return d === "osd" ? "Disk-level" : "Node-level";
}

function redundancyLabel(size: number, domain: string): string {
  if (size === 1) return "No redundancy — a single disk failure loses data";
  if (size === 2 && domain === "osd") return "Survives 1 disk failure";
  if (size === 2 && domain === "host") return "Survives 1 full node going offline";
  if (size === 3 && domain === "osd") return "Survives 2 disk failures";
  if (size === 3 && domain === "host") return "Survives 2 full nodes going offline";
  return "";
}

type Domain = "osd" | "host";

function ReplicationPanel({ pools, osds }: { pools: PoolInfo[]; osds: OsdInfo[] }) {
  const cephFs = pools.filter(p => !p.name.startsWith("."));
  const current = cephFs[0];

  const [domain, setDomain] = useState<Domain>((current?.failure_domain as Domain) ?? "osd");
  const [size, setSize]     = useState<number>(current?.size ?? 2);
  const [applying, setApplying] = useState(false);
  const [confirm, setConfirm]   = useState(false);
  const [result, setResult]     = useState<string | null>(null);

  const changed = current && (domain !== current.failure_domain || size !== current.size);
  const estimate = estimateUsable(osds, size, domain);

  async function apply() {
    setApplying(true);
    setResult(null);
    try {
      const r = await fetch("/api/ceph/replication", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ size, min_size: 1, failure_domain: domain }),
      });
      const d = await r.json() as { ok?: boolean; error?: string };
      setResult(d.ok ? "Applied — backfill started." : (d.error ?? "Unknown error"));
      setConfirm(false);
    } catch (e) {
      setResult(String(e));
    } finally {
      setApplying(false);
    }
  }

  const authoritative = cephFs[0]?.max_avail_bytes ?? 0;
  const showEstimate  = changed || authoritative === 0;

  return (
    <Card>
      <CardHeader>
        <CardTitle>Replication</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">

        {/* Current pool summary */}
        {cephFs.length > 0 && (
          <div className="rounded-md border border-[#27272a] divide-y divide-[#27272a]/50">
            {cephFs.map(p => (
              <div key={p.id} className="flex items-center gap-4 px-4 py-2.5 text-xs">
                <span className="font-mono text-[#a78bfa] w-40 truncate">{p.name}</span>
                <span className="text-[#71717a]">size={p.size} / min={p.min_size}</span>
                <span className="text-[#71717a]">{domainLabel(p.failure_domain)}</span>
                <span className="text-[#52525b] ml-auto">{p.crush_rule_name}</span>
                <span className="text-[#4ade80] font-medium">max {fmtBytes(p.max_avail_bytes)}</span>
              </div>
            ))}
          </div>
        )}

        {/* Controls */}
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-6">
          <div className="space-y-2">
            <p className="text-xs font-medium text-[#71717a] uppercase tracking-wider">Failure domain</p>
            <div className="flex gap-2">
              {(["osd", "host"] as Domain[]).map(d => (
                <button
                  key={d}
                  onClick={() => { setDomain(d); setConfirm(false); setResult(null); }}
                  className={cn(
                    "flex-1 rounded-md border px-3 py-2 text-sm transition-colors",
                    domain === d
                      ? "border-[#a78bfa] bg-[#a78bfa]/10 text-[#c4b5fd]"
                      : "border-[#27272a] text-[#71717a] hover:border-[#3f3f46] hover:text-[#a1a1aa]",
                  )}
                >
                  {d === "osd" ? "Disk-level" : "Node-level"}
                </button>
              ))}
            </div>
            <p className="text-xs text-[#52525b]">
              {domain === "osd"
                ? "Copies land on any 2 different disks — may be on the same node."
                : "Each copy must land on a different node — survives a full node outage."}
            </p>
          </div>

          <div className="space-y-2">
            <p className="text-xs font-medium text-[#71717a] uppercase tracking-wider">Copies (replication size)</p>
            <div className="flex gap-2">
              {[1, 2, 3].map(s => (
                <button
                  key={s}
                  onClick={() => { setSize(s); setConfirm(false); setResult(null); }}
                  className={cn(
                    "flex-1 rounded-md border px-3 py-2 text-sm transition-colors",
                    size === s
                      ? "border-[#a78bfa] bg-[#a78bfa]/10 text-[#c4b5fd]"
                      : "border-[#27272a] text-[#71717a] hover:border-[#3f3f46] hover:text-[#a1a1aa]",
                  )}
                >
                  {s}×
                </button>
              ))}
            </div>
            <p className={cn(
              "text-xs",
              size === 1 ? "text-[#f87171]" : "text-[#52525b]",
            )}>
              {redundancyLabel(size, domain)}
            </p>
          </div>
        </div>

        {/* Capacity estimate */}
        <div className="rounded-md bg-[#18181b] border border-[#27272a] px-4 py-3 flex items-center gap-4 flex-wrap">
          <div>
            <p className="text-xs text-[#71717a]">
              {showEstimate ? "Estimated usable capacity" : "Usable capacity (Ceph)"}
            </p>
            <p className="text-xl font-semibold text-[#fafafa] mt-0.5">
              {showEstimate ? `~${fmtBytes(estimate)}` : fmtBytes(authoritative)}
            </p>
          </div>
          {showEstimate && (
            <p className="text-xs text-[#52525b] flex-1">
              Approximate — apply to get Ceph's exact answer.
              {domain === "host" && (() => {
                const hostMap = new Map<string, number>();
                for (const o of osds) hostMap.set(o.host, (hostMap.get(o.host) ?? 0) + o.size_bytes);
                const caps = [...hostMap.values()];
                const unbalanced = caps.length >= 2 && Math.max(...caps) / Math.min(...caps) > 2;
                return unbalanced ? " ⚠ Nodes are unbalanced — the smallest node caps your capacity." : null;
              })()}
            </p>
          )}
          {!showEstimate && (
            <p className="text-xs text-[#52525b]">Exact figure from Ceph. Changes live as data is written.</p>
          )}
        </div>

        {/* Apply */}
        {changed && !confirm && !result && (
          <Button onClick={() => setConfirm(true)} variant="outline" className="gap-2">
            Apply changes
          </Button>
        )}

        {confirm && (
          <div className="rounded-md border border-[#fbbf24]/30 bg-[#fbbf24]/5 px-4 py-3 space-y-3">
            <div className="flex items-start gap-2">
              <AlertTriangle className="h-4 w-4 text-[#fbbf24] mt-0.5 flex-shrink-0" />
              <div className="text-sm text-[#fbbf24]">
                <p className="font-medium">Apply {size}× replication, {domainLabel(domain)}?</p>
                <p className="text-xs text-[#fde68a] mt-0.5">
                  Ceph will immediately start rebalancing. Estimated usable: ~{fmtBytes(estimate)}.
                </p>
              </div>
            </div>
            <div className="flex gap-2">
              <Button onClick={() => void apply()} disabled={applying} size="sm" className="gap-2">
                {applying && <RefreshCw className="h-3.5 w-3.5 animate-spin" />}
                {applying ? "Applying…" : "Confirm"}
              </Button>
              <Button onClick={() => setConfirm(false)} disabled={applying} size="sm" variant="outline">
                Cancel
              </Button>
            </div>
          </div>
        )}

        {result && (
          <p className={cn(
            "text-sm",
            result.startsWith("Applied") ? "text-[#4ade80]" : "text-[#f87171]",
          )}>
            {result}
          </p>
        )}
      </CardContent>
    </Card>
  );
}

export function StoragePage() {
  const [detail, setDetail] = useState<StorageDetail | null>(null);
  const [error, setError]   = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  function load() {
    setLoading(true);
    fetch("/api/ceph/detail")
      .then(r => r.json())
      .then((d: StorageDetailResponse) => {
        if (d.ok && d.data) { setDetail(d.data); setError(null); }
        else setError(d.error ?? "Unknown error");
      })
      .catch(e => setError(String(e)))
      .finally(() => setLoading(false));
  }

  useEffect(() => { load(); }, []);

  return (
    <div className="space-y-6 max-w-5xl">
      <div className="flex items-start justify-between">
        <div>
          <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
          <p className="text-sm text-[#71717a] mt-0.5">Disk layout, capacity, and replication settings</p>
        </div>
        <Button variant="outline" size="sm" onClick={load} disabled={loading} className="gap-2 mt-0.5">
          <RefreshCw className={cn("h-3.5 w-3.5", loading && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {error && (
        <div className="flex items-start gap-2 rounded-lg border border-[#f87171]/30 bg-[#f87171]/5 p-4">
          <AlertTriangle className="h-4 w-4 text-[#f87171] mt-0.5 flex-shrink-0" />
          <p className="text-sm text-[#f87171]">{error}</p>
        </div>
      )}

      {loading && !detail && (
        <p className="text-sm text-[#71717a]">Loading storage data…</p>
      )}

      {detail && (
        <>
          {/* Cluster totals */}
          <div className="grid grid-cols-3 gap-3">
            {[
              { label: "Raw total", value: fmtBytes(detail.total_bytes) },
              { label: "Raw used",  value: fmtBytes(detail.used_bytes)  },
              { label: "Raw free",  value: fmtBytes(detail.avail_bytes) },
            ].map(({ label, value }) => (
              <Card key={label}>
                <CardContent className="pt-4 pb-3">
                  <p className="text-xs text-[#71717a]">{label}</p>
                  <p className="text-lg font-semibold text-[#fafafa] mt-0.5 tabular-nums">{value}</p>
                </CardContent>
              </Card>
            ))}
          </div>

          <OsdTable osds={detail.osds} />
          <ReplicationPanel pools={detail.pools} osds={detail.osds} />
        </>
      )}
    </div>
  );
}
