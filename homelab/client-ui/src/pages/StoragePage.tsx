import { useEffect, useState } from "react";
import { RefreshCw, HardDrive, AlertTriangle, ChevronDown, Copy, Check, ExternalLink, Eye, EyeOff, Loader2 } from "lucide-react";
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

// ── OSD lifecycle state ────────────────────────────────────────────────────────

type OsdState = "active" | "offline" | "new" | "draining" | "inactive";

function getOsdState(osd: OsdInfo): OsdState {
  if (osd.crush_weight > 0) {
    return osd.status === "up" ? "active" : "offline";
  }
  // crush_weight === 0
  if (osd.reweight > 0.5) return "new";       // just joined; watcher will mark out
  if (osd.safe_to_destroy)  return "inactive"; // no data; ready to activate or unplug
  return "draining";                           // data still migrating off
}

const STATE_CONFIG: Record<OsdState, { label: string; variant: "success" | "destructive" | "warning" | "muted" }> = {
  active:   { label: "Active",   variant: "success" },
  offline:  { label: "Offline",  variant: "destructive" },
  new:      { label: "Pending",  variant: "warning" },
  draining: { label: "Draining", variant: "warning" },
  inactive: { label: "Inactive", variant: "muted" },
};

function OsdStateBadge({ osd }: { osd: OsdInfo }) {
  const state = getOsdState(osd);
  const { label, variant } = STATE_CONFIG[state];
  return (
    <Badge variant={variant} className="gap-1.5">
      {state === "draining" && <Loader2 className="h-2.5 w-2.5 animate-spin" />}
      {label}
      {state === "draining" && osd.pgs > 0 && (
        <span className="opacity-70">• {osd.pgs} PG{osd.pgs !== 1 ? "s" : ""}</span>
      )}
    </Badge>
  );
}

function OsdActions({ osd, onRefresh }: { osd: OsdInfo; onRefresh: () => void }) {
  const state = getOsdState(osd);
  const [busy, setBusy]       = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [err, setErr]         = useState<string | null>(null);

  async function callApi(path: string) {
    setBusy(true);
    setErr(null);
    try {
      const r = await fetch(path, { method: "POST" });
      const d = await r.json() as { ok?: boolean; error?: string };
      if (!d.ok) setErr(d.error ?? "Unknown error");
      else onRefresh();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
      setConfirm(false);
    }
  }

  if (state === "inactive") {
    return (
      <div className="flex flex-col gap-1">
        <Button size="sm" variant="outline" disabled={busy}
          onClick={() => void callApi(`/api/ceph/osd/${osd.id}/activate`)}>
          {busy ? <Loader2 className="h-3 w-3 animate-spin" /> : "Activate"}
        </Button>
        {err && <p className="text-xs text-[#f87171]">{err}</p>}
      </div>
    );
  }

  if (state === "active" || state === "offline") {
    if (confirm) {
      return (
        <div className="flex flex-col gap-1">
          <div className="flex gap-1.5">
            <Button size="sm" variant="destructive" disabled={busy}
              onClick={() => void callApi(`/api/ceph/osd/${osd.id}/deactivate`)}>
              {busy ? <Loader2 className="h-3 w-3 animate-spin" /> : "Confirm"}
            </Button>
            <Button size="sm" variant="ghost" disabled={busy} onClick={() => setConfirm(false)}>
              Cancel
            </Button>
          </div>
          {err && <p className="text-xs text-[#f87171]">{err}</p>}
        </div>
      );
    }
    return (
      <Button size="sm" variant="outline" onClick={() => setConfirm(true)}>
        Deactivate
      </Button>
    );
  }

  return null;
}

function OsdTable({ osds, onRefresh }: { osds: OsdInfo[]; onRefresh: () => void }) {
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
                {["OSD", "Host", "Class", "Size", "Fill", "Used / Free", "PGs", "Balance", "State", ""].map((h, i) => (
                  <th key={i} className="px-4 py-2.5 text-left text-xs font-medium text-[#71717a] whitespace-nowrap first:pl-6 last:pr-6">{h}</th>
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
                      {osd.size_bytes > 0 ? fmtBytes(osd.size_bytes) : "—"}
                    </td>
                    <td className="px-4 py-3">
                      {osd.utilization > 0
                        ? <FillBar pct={osd.utilization} />
                        : <span className="text-xs text-[#3f3f46]">—</span>}
                    </td>
                    <td className="px-4 py-3 text-xs text-[#71717a] whitespace-nowrap tabular-nums">
                      {osd.used_bytes > 0
                        ? `${fmtBytes(osd.used_bytes)} / ${fmtBytes(osd.avail_bytes)}`
                        : "—"}
                    </td>
                    <td className="px-4 py-3 text-xs text-[#71717a] tabular-nums">{osd.pgs}</td>
                    <td className="px-4 py-3"><VarBadge v={osd.var} /></td>
                    <td className="px-4 py-3"><OsdStateBadge osd={osd} /></td>
                    <td className="pr-6 px-4 py-3">
                      <OsdActions osd={osd} onRefresh={onRefresh} />
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

type Domain = "osd" | "host";

function ReplicationPanel({ pools, osds }: { pools: PoolInfo[]; osds: OsdInfo[] }) {
  const cephFs = pools.filter(p => !p.name.startsWith("."));
  const current = cephFs[0];

  const nDisks = osds.length;
  const nNodes = new Set(osds.map(o => o.host)).size;

  const [domain, setDomain] = useState<Domain>((current?.failure_domain as Domain) ?? "osd");
  const [size, setSize]     = useState<number>(current?.size ?? 2);
  const [applying, setApplying] = useState(false);
  const [confirm, setConfirm]   = useState(false);
  const [result, setResult]     = useState<string | null>(null);

  const maxSize = domain === "osd" ? nDisks : nNodes;

  function changeDomain(d: Domain) {
    setDomain(d);
    const newMax = d === "osd" ? nDisks : nNodes;
    if (size > newMax) setSize(newMax);
    setConfirm(false);
    setResult(null);
  }

  const changed = current && (domain !== current.failure_domain || size !== current.size);
  const authoritative = current?.max_avail_bytes ?? 0;
  const showEstimate  = changed || authoritative === 0;
  const displayCapacity = showEstimate ? estimateUsable(osds, size, domain) : authoritative;

  const survivesDisks    = size - 1;
  const survivesMachines = domain === "host" ? size - 1 : 0;

  // Feasibility: increasing replication requires (delta_copies × logical_stored) of free raw space.
  // stored_bytes is the logical (deduplicated, one-copy) size; raw free is sum of OSD avail.
  const totalStored = cephFs.reduce((s, p) => s + p.stored_bytes, 0);
  const currentSize = current?.size ?? 1;
  const rawFree     = osds.reduce((s, o) => s + o.avail_bytes, 0);
  const rawNeeded   = (size - currentSize) * totalStored;  // negative = freeing space
  type Feasibility  = "ok" | "tight" | "impossible" | null;
  let feasibility: Feasibility = null;
  if (rawNeeded > 0) {
    if (rawFree < rawNeeded)           feasibility = "impossible";
    else if (rawFree < rawNeeded * 1.3) feasibility = "tight";
    else                                feasibility = "ok";
  }

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

  return (
    <Card>
      <CardHeader>
        <CardTitle>Replication</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">

        {/* Controls */}
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-6">
          <div className="space-y-2">
            <p className="text-xs font-medium text-[#71717a] uppercase tracking-wider">Redundancy scope</p>
            <div className="flex gap-2">
              {(["osd", "host"] as Domain[]).map(d => (
                <button
                  key={d}
                  onClick={() => changeDomain(d)}
                  className={cn(
                    "flex-1 rounded-md border px-3 py-2 text-sm transition-colors",
                    domain === d
                      ? "border-[#a78bfa] bg-[#a78bfa]/10 text-[#c4b5fd]"
                      : "border-[#27272a] text-[#71717a] hover:border-[#3f3f46] hover:text-[#a1a1aa]",
                  )}
                >
                  {d === "osd" ? "Disk-level" : "Machine-level"}
                </button>
              ))}
            </div>
          </div>

          <div className="space-y-2">
            <p className="text-xs font-medium text-[#71717a] uppercase tracking-wider">
              Copies
              <span className="ml-1.5 normal-case font-normal text-[#52525b]">
                (max {maxSize} — you have {domain === "osd" ? `${nDisks} disk${nDisks !== 1 ? "s" : ""}` : `${nNodes} machine${nNodes !== 1 ? "s" : ""}`})
              </span>
            </p>
            <div className="flex gap-2">
              {[1, 2, 3].map(s => {
                const disabled = s > maxSize;
                return (
                  <button
                    key={s}
                    disabled={disabled}
                    onClick={() => { setSize(s); setConfirm(false); setResult(null); }}
                    className={cn(
                      "flex-1 rounded-md border px-3 py-2 text-sm transition-colors",
                      disabled
                        ? "border-[#27272a]/50 text-[#3f3f46] cursor-not-allowed opacity-40"
                        : size === s
                          ? "border-[#a78bfa] bg-[#a78bfa]/10 text-[#c4b5fd]"
                          : "border-[#27272a] text-[#71717a] hover:border-[#3f3f46] hover:text-[#a1a1aa]",
                    )}
                  >
                    {s}×
                  </button>
                );
              })}
            </div>
          </div>
        </div>

        {/* Feasibility warning */}
        {feasibility === "impossible" && (
          <div className="flex items-start gap-2.5 rounded-md border border-[#f87171]/30 bg-[#f87171]/5 px-4 py-3">
            <AlertTriangle className="h-4 w-4 text-[#f87171] mt-0.5 flex-shrink-0" />
            <div className="text-sm">
              <p className="font-medium text-[#f87171]">Not enough free space</p>
              <p className="text-[#fca5a5] text-xs mt-0.5">
                Creating {size - currentSize} extra {size - currentSize === 1 ? "copy" : "copies"} of your data
                needs <span className="font-semibold">{fmtBytes(rawNeeded)}</span> of free
                space — you only have <span className="font-semibold">{fmtBytes(rawFree)}</span>.
                Free up space or reduce your data before switching.
              </p>
            </div>
          </div>
        )}
        {feasibility === "tight" && (
          <div className="flex items-start gap-2.5 rounded-md border border-[#fbbf24]/30 bg-[#fbbf24]/5 px-4 py-3">
            <AlertTriangle className="h-4 w-4 text-[#fbbf24] mt-0.5 flex-shrink-0" />
            <div className="text-sm">
              <p className="font-medium text-[#fbbf24]">Tight fit</p>
              <p className="text-[#fde68a] text-xs mt-0.5">
                Rebalancing needs <span className="font-semibold">{fmtBytes(rawNeeded)}</span> and
                you have <span className="font-semibold">{fmtBytes(rawFree)}</span> free —
                less than 30% headroom. The cluster may hit nearfull warnings during backfill.
              </p>
            </div>
          </div>
        )}

        {/* Capacity + resilience */}
        <div className="rounded-md bg-[#18181b] border border-[#27272a] px-5 py-4 space-y-3">
          <div>
            <p className="text-xs text-[#71717a] mb-0.5">
              {showEstimate ? "Estimated capacity" : "Usable capacity"}
            </p>
            <p className="text-2xl font-semibold text-[#fafafa] tabular-nums">
              {showEstimate ? `~${fmtBytes(displayCapacity)}` : fmtBytes(displayCapacity)}
            </p>
          </div>
          <div className="h-px bg-[#27272a]" />
          <div className="space-y-1.5">
            <p className={cn(
              "text-sm",
              survivesDisks === 0 ? "text-[#f87171]" : "text-[#a1a1aa]",
            )}>
              Cluster can survive{" "}
              <span className={cn("font-semibold", survivesDisks === 0 ? "text-[#f87171]" : "text-[#fafafa]")}>
                {survivesDisks} disk{survivesDisks !== 1 ? "s" : ""}
              </span>{" "}
              going down
            </p>
            {domain === "host" && (
              <p className={cn(
                "text-sm",
                survivesMachines === 0 ? "text-[#f87171]" : "text-[#a1a1aa]",
              )}>
                Cluster can survive{" "}
                <span className={cn("font-semibold", survivesMachines === 0 ? "text-[#f87171]" : "text-[#fafafa]")}>
                  {survivesMachines} machine{survivesMachines !== 1 ? "s" : ""}
                </span>{" "}
                going down
              </p>
            )}
          </div>
        </div>

        {/* Apply */}
        {changed && !confirm && !result && (
          <Button
            onClick={() => setConfirm(true)}
            disabled={feasibility === "impossible"}
            variant="outline"
            className="gap-2"
          >
            Apply changes
          </Button>
        )}

        {confirm && (
          <div className="rounded-md border border-[#fbbf24]/30 bg-[#fbbf24]/5 px-4 py-3 space-y-3">
            <div className="flex items-start gap-2">
              <AlertTriangle className="h-4 w-4 text-[#fbbf24] mt-0.5 flex-shrink-0" />
              <p className="text-sm text-[#fbbf24] font-medium">
                Apply {size}× replication, {domain === "osd" ? "disk-level" : "machine-level"}? Ceph will start rebalancing immediately.
              </p>
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

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  function copy() {
    void navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }
  return (
    <button onClick={copy} className="text-[#52525b] hover:text-[#a1a1aa] transition-colors">
      {copied ? <Check className="h-3.5 w-3.5 text-[#4ade80]" /> : <Copy className="h-3.5 w-3.5" />}
    </button>
  );
}

function AdvancedPanel() {
  const [open, setOpen]         = useState(false);
  const [loading, setLoading]   = useState(false);
  const [creds, setCreds]       = useState<{ username: string; password: string } | null>(null);
  const [showPass, setShowPass] = useState(false);

  function toggle() {
    if (!open && !creds) {
      setLoading(true);
      fetch("/api/ceph/dashboard")
        .then(r => r.json())
        .then(d => setCreds(d))
        .catch(() => {})
        .finally(() => setLoading(false));
    }
    setOpen(o => !o);
  }

  return (
    <div className="rounded-md border border-[#27272a] overflow-hidden">
      <button
        onClick={toggle}
        className="w-full flex items-center justify-between px-4 py-3 text-sm text-[#71717a] hover:text-[#a1a1aa] hover:bg-[#18181b] transition-colors"
      >
        <span className="font-medium">Advanced</span>
        <ChevronDown className={cn("h-4 w-4 transition-transform", open && "rotate-180")} />
      </button>

      {open && (
        <div className="border-t border-[#27272a] px-4 py-4 bg-[#111114] space-y-4">
          {loading && <p className="text-xs text-[#52525b]">Loading…</p>}
          {creds && (
            <>
              <div className="space-y-3">
                {/* Username */}
                <div className="flex items-center justify-between">
                  <span className="text-xs text-[#71717a] w-24">Username</span>
                  <div className="flex items-center gap-2 font-mono text-sm text-[#fafafa]">
                    {creds.username}
                    <CopyButton text={creds.username} />
                  </div>
                </div>
                {/* Password */}
                <div className="flex items-center justify-between">
                  <span className="text-xs text-[#71717a] w-24">Password</span>
                  <div className="flex items-center gap-2 font-mono text-sm text-[#fafafa]">
                    {showPass ? creds.password : "••••••••••••"}
                    <button
                      onClick={() => setShowPass(s => !s)}
                      className="text-[#52525b] hover:text-[#a1a1aa] transition-colors"
                    >
                      {showPass ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                    </button>
                    <CopyButton text={creds.password} />
                  </div>
                </div>
                {/* Dashboard link */}
                <div className="flex items-center justify-between">
                  <span className="text-xs text-[#71717a] w-24">Dashboard</span>
                  <a
                    href="/ceph-dashboard/"
                    target="_blank"
                    rel="noopener noreferrer"
                    className="flex items-center gap-1.5 text-sm text-[#a78bfa] hover:text-[#c4b5fd] transition-colors"
                  >
                    Open Ceph dashboard
                    <ExternalLink className="h-3.5 w-3.5" />
                  </a>
                </div>
              </div>
            </>
          )}
        </div>
      )}
    </div>
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

          <OsdTable osds={detail.osds} onRefresh={load} />
          <ReplicationPanel pools={detail.pools} osds={detail.osds} />
        </>
      )}

      <AdvancedPanel />
    </div>
  );
}
