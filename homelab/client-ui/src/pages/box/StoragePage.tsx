import { useEffect, useState } from "react";
import {
  RefreshCw,
  HardDrive,
  AlertTriangle,
  ChevronDown,
  Copy,
  Check,
  ExternalLink,
  Eye,
  EyeOff,
  Loader2,
  Cpu,
  WifiOff,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import type {
  OsdInfo,
  PoolInfo,
  StorageDetail,
  StorageDetailResponse,
  DiskInfo,
  StoragePolicyData,
} from "@/types/storage";

const GiB = 1073741824;
const TiB = GiB * 1024;

function fmtBytes(b: number): string {
  if (b >= TiB) return `${(b / TiB).toFixed(2)} TiB`;
  if (b >= GiB) return `${(b / GiB).toFixed(1)} GiB`;
  if (b >= 1048576) return `${(b / 1048576).toFixed(0)} MiB`;
  return `${(b / 1024).toFixed(0)} KiB`;
}

function fmtSize(b: number): string {
  if (b >= 1e12) return `${(b / 1e12).toFixed(1)} TB`;
  if (b >= 1e9) return `${(b / 1e9).toFixed(0)} GB`;
  if (b >= 1e6) return `${(b / 1e6).toFixed(0)} MB`;
  return `${b} B`;
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
      <div className="flex-1 h-1.5 rounded-full bg-border overflow-hidden">
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
    <span
      className={cn(
        "text-xs tabular-nums font-mono",
        ok ? "text-fg-muted" : warn ? "text-warning" : "text-danger",
      )}
    >
      {v.toFixed(2)}
    </span>
  );
}

function OsdPill({ on, labels }: { on: boolean; labels: [string, string] }) {
  return (
    <Badge variant={on ? "success" : "muted"}>
      {on ? labels[0] : labels[1]}
    </Badge>
  );
}

function OsdActions({
  osd,
  onRefresh,
}: {
  osd: OsdInfo;
  onRefresh: () => void;
}) {
  const isIn = osd.crush_weight > 0;
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  async function callApi(path: string) {
    setBusy(true);
    setErr(null);
    try {
      const r = await fetch(path, { method: "POST" });
      const d = (await r.json()) as { ok?: boolean; error?: string };
      if (!d.ok) setErr(d.error ?? "Unknown error");
      else {
        setConfirm(false);
        onRefresh();
      }
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-1">
      {isIn ? (
        confirm ? (
          <div className="flex gap-1.5">
            <Button
              size="sm"
              variant="danger"
              disabled={busy}
              onClick={() => void callApi(`/api/ceph/osd/${osd.id}/mark-out`)}
            >
              {busy ? <Loader2 className="h-3 w-3 animate-spin" /> : "Confirm"}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              disabled={busy}
              onClick={() => setConfirm(false)}
            >
              Cancel
            </Button>
          </div>
        ) : (
          <Button size="sm" variant="outline" onClick={() => setConfirm(true)}>
            Remove safely
          </Button>
        )
      ) : (
        <Button
          size="sm"
          variant="outline"
          disabled={busy}
          onClick={() => void callApi(`/api/ceph/osd/${osd.id}/mark-in`)}
        >
          {busy ? <Loader2 className="h-3 w-3 animate-spin" /> : "Re-add disk"}
        </Button>
      )}
      {err && <p className="text-xs text-danger">{err}</p>}
    </div>
  );
}

type DiskState =
  | "active"
  | "pending"
  | "missing"
  | "draining"
  | "excluded"
  | "historical"
  | "foreign";

function diskState(disk: DiskInfo): DiskState {
  if (disk.foreign_ceph) return "foreign";
  const on = disk.desired === "ON" || disk.desired === "USING";
  if (on && disk.connected && disk.is_our_osd) return "active";
  if (on && disk.connected) return "pending";
  if (on && !disk.connected) return "missing";
  if (!on && disk.connected && disk.is_our_osd) return "draining";
  if (!on && disk.connected) return "excluded";
  return "historical";
}

const STATE_META: Record<
  DiskState,
  { label: string; color: string; dot: string; pulse?: boolean }
> = {
  active: {
    label: "Active in cluster",
    color: "text-success",
    dot: "bg-success",
  },
  pending: {
    label: "Setting up…",
    color: "text-warning",
    dot: "bg-warning",
    pulse: true,
  },
  missing: {
    label: "Missing — not connected",
    color: "text-danger",
    dot: "bg-danger",
    pulse: true,
  },
  draining: {
    label: "Draining — being removed",
    color: "text-warning",
    dot: "bg-warning",
    pulse: true,
  },
  excluded: {
    label: "Connected, not in cluster",
    color: "text-fg-muted",
    dot: "bg-fg-subtle",
  },
  historical: {
    label: "Not connected",
    color: "text-fg-subtle",
    dot: "bg-border-strong",
  },
  foreign: {
    label: "Foreign data — erase to use",
    color: "text-warning",
    dot: "bg-warning",
  },
};

function DiskRow({
  node,
  disk,
  osd,
  onChanged,
}: {
  node: string;
  disk: DiskInfo;
  osd?: OsdInfo;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [eraseConfirm, setEraseConfirm] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const state = diskState(disk);
  const sm = STATE_META[state];
  const isOn = disk.desired === "ON" || disk.desired === "USING";

  async function toggle() {
    const next = isOn ? "OFF" : "ON";
    if (next === "OFF" && disk.is_our_osd && !confirm) {
      setConfirm(true);
      return;
    }
    setBusy(true);
    setErr(null);
    setConfirm(false);
    try {
      const r = await fetch(`/api/disks/${node}/${disk.id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ desired: next }),
      });
      const d = (await r.json()) as { ok?: boolean; error?: string };
      if (!d.ok) setErr(d.error ?? "Unknown error");
      else onChanged();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function erase() {
    setBusy(true);
    setErr(null);
    setEraseConfirm(false);
    try {
      const r = await fetch(`/api/disks/${node}/${disk.id}/erase`, {
        method: "POST",
      });
      const d = (await r.json()) as { ok?: boolean; error?: string };
      if (!d.ok) setErr(d.error ?? "Erase failed");
      else onChanged();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  const label = disk.model || disk.device || disk.id;
  const isMissing = state === "missing";
  const isHistorical = state === "historical";

  return (
    <div
      className={cn(
        "flex items-center gap-4 py-3 px-4 border-b border-border/50 last:border-0 transition-colors",
        isMissing && "bg-danger/5 border-danger/20",
        isHistorical && "opacity-50",
      )}
    >
      {/* Icon */}
      <div className="flex-shrink-0">
        {state === "missing" ? (
          <WifiOff className="h-4 w-4 text-danger" strokeWidth={1.5} />
        ) : disk.is_loop ? (
          <Cpu className="h-4 w-4 text-primary" strokeWidth={1.5} />
        ) : (
          <HardDrive
            className={cn(
              "h-4 w-4",
              isHistorical ? "text-border-strong" : "text-fg-muted",
            )}
            strokeWidth={1.5}
          />
        )}
      </div>

      {/* Identity + state */}
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 flex-wrap">
          <p
            className={cn(
              "text-sm truncate",
              isHistorical ? "text-fg-subtle" : "text-fg",
            )}
          >
            {label}
          </p>
          {disk.is_our_osd && disk.osd_id !== null && disk.connected && (
            <span className="text-xs font-mono text-primary shrink-0">
              osd.{disk.osd_id}
            </span>
          )}
        </div>
        <div className="flex items-center gap-1.5 mt-0.5">
          <span
            className={cn(
              "inline-block h-1.5 w-1.5 rounded-full shrink-0",
              sm.dot,
              sm.pulse && "animate-pulse",
            )}
          />
          <p className={cn("text-xs", sm.color)}>
            {sm.label}
            {disk.connected && disk.size_bytes > 0 && (
              <span className="text-fg-subtle ml-1.5">
                · {fmtSize(disk.size_bytes)}
              </span>
            )}
            {disk.is_loop && (
              <span className="text-fg-subtle ml-1.5">· System disk</span>
            )}
          </p>
        </div>
      </div>

      {/* OSD fill stats — only for active disks */}
      {osd && state === "active" && (
        <div className="hidden sm:flex flex-col items-end gap-0.5 shrink-0 min-w-[140px]">
          <FillBar pct={osd.utilization} />
          <p className="text-xs text-fg-subtle tabular-nums">
            {fmtBytes(osd.used_bytes)} / {fmtBytes(osd.avail_bytes)}
          </p>
        </div>
      )}

      {/* Foreign disk: erase flow */}
      {state === "foreign" && (
        <div className="shrink-0">
          {eraseConfirm ? (
            <div className="flex items-center gap-1.5">
              <span className="text-xs text-danger">Erase all data?</span>
              <button
                onClick={() => void erase()}
                disabled={busy}
                className="px-2 py-0.5 rounded text-xs border border-danger/40 text-danger hover:bg-danger/10 transition-colors disabled:opacity-50"
              >
                {busy ? "…" : "Yes"}
              </button>
              <button
                onClick={() => setEraseConfirm(false)}
                className="px-2 py-0.5 rounded text-xs border border-border text-fg-muted hover:bg-border transition-colors"
              >
                No
              </button>
            </div>
          ) : (
            <button
              onClick={() => setEraseConfirm(true)}
              disabled={busy}
              className="px-3 py-1 rounded text-xs border border-border text-fg-muted hover:border-border-strong hover:text-fg transition-colors disabled:opacity-50"
            >
              Erase and use
            </button>
          )}
        </div>
      )}

      {/* Confirm removing active OSD */}
      {confirm && (
        <div className="flex items-center gap-1.5 text-xs shrink-0">
          <span className="text-warning hidden sm:inline">
            Data will migrate first.
          </span>
          <button
            onClick={() => void toggle()}
            disabled={busy}
            className="px-2 py-0.5 rounded border border-danger/40 text-danger hover:bg-danger/10 transition-colors"
          >
            Confirm
          </button>
          <button
            onClick={() => setConfirm(false)}
            className="px-2 py-0.5 rounded border border-border text-fg-muted hover:bg-border transition-colors"
          >
            Cancel
          </button>
        </div>
      )}

      {/* ON/OFF toggle — not shown for foreign disks or during confirm */}
      {state !== "foreign" && !confirm && (
        <button
          onClick={() => void toggle()}
          disabled={busy}
          title={isOn ? "Turn OFF" : "Turn ON"}
          className={cn(
            "flex-shrink-0 relative inline-flex h-5 w-9 items-center rounded-full transition-colors",
            isOn ? "bg-primary" : "bg-border-strong",
            busy && "opacity-50 cursor-not-allowed",
          )}
        >
          <span
            className={cn(
              "inline-block h-3.5 w-3.5 rounded-full shadow transition-transform",
              isOn ? "translate-x-5 bg-white" : "translate-x-0.5 bg-white/70",
            )}
          />
        </button>
      )}

      {err && <p className="text-xs text-danger ml-1 shrink-0">{err}</p>}
    </div>
  );
}

function ManageDisksPanel({ osds }: { osds: OsdInfo[] }) {
  const [disks, setDisks] = useState<Record<string, DiskInfo[]> | null>(null);
  const [loading, setLoading] = useState(true);

  function load() {
    setLoading(true);
    fetch("/api/disks")
      .then((r) => r.json())
      .then((d) => setDisks(d))
      .catch(() => {})
      .finally(() => setLoading(false));
  }

  useEffect(() => {
    load();
  }, []);

  const nodes = disks ? Object.keys(disks).sort() : [];

  function getOsd(disk: DiskInfo): OsdInfo | undefined {
    if (disk.osd_id === null) return undefined;
    return osds.find((o) => o.id === disk.osd_id);
  }

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center justify-between">
          <CardTitle>Your disks</CardTitle>
          <button
            onClick={load}
            disabled={loading}
            className="text-fg-subtle hover:text-fg-muted transition-colors"
          >
            <RefreshCw
              className={cn("h-3.5 w-3.5", loading && "animate-spin")}
            />
          </button>
        </div>
        <p className="text-xs text-fg-subtle mt-0.5">
          All disks ever connected to this machine appear here. Turn a disk ON
          to add it to the storage pool. New disks default to OFF — nothing
          happens until you enable them.
        </p>
      </CardHeader>
      <CardContent className="p-0">
        {loading && !disks && (
          <p className="text-sm text-fg-muted px-4 py-3">Scanning disks…</p>
        )}
        {disks && nodes.length === 0 && (
          <p className="text-sm text-fg-muted px-4 py-3">
            No disks detected yet.
          </p>
        )}
        {nodes.map((node) => (
          <div key={node}>
            <p className="text-xs font-medium text-fg-muted uppercase tracking-wider px-4 pt-3 pb-1 flex items-center gap-1.5">
              <HardDrive className="h-3 w-3" />
              {node}
            </p>
            {(disks![node] ?? []).map((disk) => (
              <DiskRow
                key={disk.id}
                node={node}
                disk={disk}
                osd={getOsd(disk)}
                onChanged={load}
              />
            ))}
          </div>
        ))}
      </CardContent>
    </Card>
  );
}

// ── Durability posture card ────────────────────────────────────────────────────

function DurabilityCard({
  data,
  detail,
}: {
  data: StoragePolicyData;
  detail: StorageDetail | null;
}) {
  const { target, topology, policy } = data;

  const osdsDown = detail?.osds.filter((o) => o.status !== "up").length ?? 0;

  let health: "good" | "warning" | "degraded";
  if (osdsDown > 0 && target.size <= 1) health = "degraded";
  else if (osdsDown > 0) health = "warning";
  else health = "good";

  let headline: string;
  let subtext: string;
  if (target.size === 1) {
    headline = "No disk redundancy";
    subtext =
      topology.osds >= 2
        ? "Your data has one copy — a disk failure would cause data loss. Switch to Manual mode below to enable redundancy."
        : "Your data has one copy. Connect more disks to enable redundancy. Backups are your safety net.";
  } else if (target.failure_domain === "osd") {
    const survive = target.size - 1;
    headline = `${target.size} copies across ${target.size} disk${target.size > 1 ? "s" : ""}`;
    subtext =
      survive === 1
        ? `One disk can fail without data loss. You have ${topology.osds} disk${topology.osds !== 1 ? "s" : ""} on ${topology.nodes} machine${topology.nodes !== 1 ? "s" : ""}.`
        : `${survive} disks can fail simultaneously without data loss.`;
  } else {
    const survive = target.size - 1;
    headline = `${target.size} copies across ${target.size} machine${target.size > 1 ? "s" : ""}`;
    subtext =
      survive === 1
        ? `One machine can go offline without any data loss. You have ${topology.nodes} machines with ${topology.osds} total disks.`
        : `${survive} machines can go offline simultaneously without data loss.`;
  }

  if (osdsDown > 0) {
    subtext += ` (${osdsDown} disk${osdsDown > 1 ? "s" : ""} currently offline)`;
  }

  const dotColor = { good: "#4ade80", warning: "#fbbf24", degraded: "#f87171" }[
    health
  ];
  const borderCls = {
    good: "border-success/20",
    warning: "border-warning/30",
    degraded: "border-danger/30",
  }[health];
  const bgCls = { good: "", warning: "", degraded: "bg-danger/5" }[health];

  const modeLabel = policy.mode === "auto" ? "Auto" : "Manual";
  const scopeLabel =
    target.failure_domain === "host" ? "machine redundancy" : "disk redundancy";

  return (
    <div className={cn("rounded-lg border px-5 py-4", borderCls, bgCls)}>
      <div className="flex items-start gap-3">
        <span
          className="mt-2 inline-block h-2 w-2 rounded-full flex-shrink-0"
          style={{ background: dotColor }}
        />
        <div className="flex-1 min-w-0">
          <div className="flex items-baseline gap-3 flex-wrap">
            <p className="text-base font-semibold text-fg">{headline}</p>
            <span className="text-xs text-fg-subtle">
              {modeLabel} · {scopeLabel}
            </span>
          </div>
          <p className="text-sm text-fg-muted mt-1">{subtext}</p>
        </div>
      </div>
    </div>
  );
}

// ── Storage mode panel (auto/manual + redundancy controls) ────────────────────

type Domain = "osd" | "host";

function estimateUsable(
  osds: OsdInfo[],
  size: number,
  domain: "osd" | "host",
): number {
  const SAFETY = 0.95;
  const totalRaw = osds.reduce((s, o) => s + o.size_bytes, 0);
  if (domain === "osd") {
    if (osds.length <= size)
      return Math.min(...osds.map((o) => o.size_bytes)) * SAFETY;
    return (totalRaw / size) * SAFETY;
  } else {
    const hostMap = new Map<string, number>();
    for (const o of osds)
      hostMap.set(o.host, (hostMap.get(o.host) ?? 0) + o.size_bytes);
    const caps = [...hostMap.values()];
    if (caps.length < size) return 0;
    if (caps.length === size) return Math.min(...caps) * SAFETY;
    return (totalRaw / size) * SAFETY;
  }
}

function StorageModePanel({
  policyData,
  osds,
  pools,
  onPolicyChanged,
}: {
  policyData: StoragePolicyData | null;
  osds: OsdInfo[];
  pools: PoolInfo[];
  onPolicyChanged: () => void;
}) {
  const [localMode, setLocalMode] = useState<"auto" | "manual" | null>(null);
  const [localSize, setLocalSize] = useState<number | null>(null);
  const [localDomain, setLocalDomain] = useState<Domain | null>(null);
  const [applying, setApplying] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const [confirm, setConfirm] = useState(false);

  // Sync local state from fetched data when it arrives
  useEffect(() => {
    if (!policyData) return;
    setLocalMode(policyData.policy.mode);
    setLocalSize(policyData.policy.size);
    setLocalDomain(policyData.policy.failure_domain as Domain);
  }, [policyData]);

  if (
    !policyData ||
    localMode === null ||
    localSize === null ||
    localDomain === null
  ) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Storage mode</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-fg-muted">Loading…</p>
        </CardContent>
      </Card>
    );
  }

  const { target, topology } = policyData;
  const nDisks = osds.length;
  const nNodes = new Set(osds.map((o) => o.host)).size;
  const maxSize = localDomain === "osd" ? nDisks : nNodes;

  // Whether anything has changed from saved policy
  const savedPolicy = policyData.policy;
  const changed =
    localMode !== savedPolicy.mode ||
    (localMode === "manual" &&
      (localSize !== savedPolicy.size ||
        localDomain !== savedPolicy.failure_domain));

  // Feasibility for manual mode
  const cephFs = pools.filter((p) => !p.name.startsWith("."));
  const totalStored = cephFs.reduce((s, p) => s + p.stored_bytes, 0);
  const currentSize = savedPolicy.size;
  const rawFree = osds.reduce((s, o) => s + o.avail_bytes, 0);
  const rawNeeded = (localSize - currentSize) * totalStored;
  type Feasibility = "ok" | "tight" | "impossible" | null;
  let feasibility: Feasibility = null;
  if (localMode === "manual" && rawNeeded > 0) {
    if (rawFree < rawNeeded) feasibility = "impossible";
    else if (rawFree < rawNeeded * 1.3) feasibility = "tight";
    else feasibility = "ok";
  }

  // Capacity estimate
  const authoritative = cephFs[0]?.max_avail_bytes ?? 0;
  const showEstimate =
    (localMode === "manual" && changed) || authoritative === 0;
  const displayCapacity = showEstimate
    ? estimateUsable(osds, localSize, localDomain)
    : authoritative;

  const survivesDisks = (localMode === "manual" ? localSize : target.size) - 1;
  const survivesMachines = localDomain === "host" ? survivesDisks : 0;

  function changeDomain(d: Domain) {
    setLocalDomain(d);
    const newMax = d === "osd" ? nDisks : nNodes;
    if (localSize! > newMax) setLocalSize(Math.max(1, newMax));
    setConfirm(false);
    setResult(null);
  }

  async function applyChanges() {
    setApplying(true);
    setResult(null);
    try {
      const body =
        localMode === "auto"
          ? { mode: "auto" }
          : {
              mode: "manual",
              size: localSize,
              min_size: Math.max(1, localSize! - 1),
              failure_domain: localDomain,
            };
      const r = await fetch("/api/storage/policy", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      const d = (await r.json()) as { ok?: boolean; error?: string };
      if (d.ok) {
        setResult("Saved — changes apply within 60 seconds.");
        setConfirm(false);
        onPolicyChanged();
      } else {
        setResult(d.error ?? "Unknown error");
      }
    } catch (e) {
      setResult(String(e));
    } finally {
      setApplying(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Storage mode</CardTitle>
      </CardHeader>
      <CardContent className="space-y-5">
        {/* Auto / Manual toggle */}
        <div className="space-y-2">
          <p className="text-xs font-medium text-fg-muted uppercase tracking-wider">
            Mode
          </p>
          <div className="flex gap-2">
            {(["auto", "manual"] as const).map((m) => (
              <button
                key={m}
                onClick={() => {
                  setLocalMode(m);
                  setConfirm(false);
                  setResult(null);
                }}
                className={cn(
                  "flex-1 rounded-md border px-4 py-2.5 text-sm text-left transition-colors",
                  localMode === m
                    ? "border-primary bg-primary/10 text-primary"
                    : "border-border text-fg-muted hover:border-border-strong hover:text-fg-muted",
                )}
              >
                <div className="font-medium">
                  {m === "auto" ? "Automatic" : "Manual"}
                </div>
                <div className="text-xs mt-0.5 opacity-70">
                  {m === "auto"
                    ? "Adjusts redundancy as you add machines and disks"
                    : "You control the exact redundancy settings"}
                </div>
              </button>
            ))}
          </div>
        </div>

        {/* Auto: show what's currently computed */}
        {localMode === "auto" && (
          <div className="rounded-md bg-surface-2 border border-border px-4 py-3 space-y-2">
            <p className="text-xs font-medium text-fg-muted uppercase tracking-wider">
              Currently managing
            </p>
            <div className="grid grid-cols-2 sm:grid-cols-4 gap-3">
              {[
                { label: "Copies", value: `${target.size}×` },
                { label: "Min copies", value: `${target.min_size}×` },
                {
                  label: "Redundancy scope",
                  value: target.failure_domain === "host" ? "Machine" : "Disk",
                },
                { label: "Monitors", value: `${target.mon}` },
              ].map(({ label, value }) => (
                <div key={label}>
                  <p className="text-xs text-fg-subtle">{label}</p>
                  <p className="text-sm font-medium text-fg mt-0.5">{value}</p>
                </div>
              ))}
            </div>
            <p className="text-xs text-fg-subtle pt-1">
              Based on {topology.nodes} machine{topology.nodes !== 1 ? "s" : ""}{" "}
              and {topology.osds} disk{topology.osds !== 1 ? "s" : ""}. The
              system raises redundancy automatically as you add hardware; it
              never silently reduces it.
            </p>
          </div>
        )}

        {/* Manual: controls */}
        {localMode === "manual" && (
          <>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-6">
              <div className="space-y-2">
                <p className="text-xs font-medium text-fg-muted uppercase tracking-wider">
                  Redundancy scope
                </p>
                <div className="flex gap-2">
                  {(["osd", "host"] as Domain[]).map((d) => (
                    <button
                      key={d}
                      onClick={() => changeDomain(d)}
                      className={cn(
                        "flex-1 rounded-md border px-3 py-2 text-sm transition-colors",
                        localDomain === d
                          ? "border-primary bg-primary/10 text-primary"
                          : "border-border text-fg-muted hover:border-border-strong hover:text-fg-muted",
                      )}
                    >
                      {d === "osd" ? "Disk-level" : "Machine-level"}
                    </button>
                  ))}
                </div>
              </div>

              <div className="space-y-2">
                <p className="text-xs font-medium text-fg-muted uppercase tracking-wider">
                  Copies
                  <span className="ml-1.5 normal-case font-normal text-fg-subtle">
                    (max {maxSize} —{" "}
                    {localDomain === "osd"
                      ? `${nDisks} disk${nDisks !== 1 ? "s" : ""}`
                      : `${nNodes} machine${nNodes !== 1 ? "s" : ""}`}
                    )
                  </span>
                </p>
                <div className="flex gap-2">
                  {[1, 2, 3].map((s) => {
                    const disabled = s > maxSize;
                    return (
                      <button
                        key={s}
                        disabled={disabled}
                        onClick={() => {
                          setLocalSize(s);
                          setConfirm(false);
                          setResult(null);
                        }}
                        className={cn(
                          "flex-1 rounded-md border px-3 py-2 text-sm transition-colors",
                          disabled
                            ? "border-border/50 text-border-strong cursor-not-allowed opacity-40"
                            : localSize === s
                              ? "border-primary bg-primary/10 text-primary"
                              : "border-border text-fg-muted hover:border-border-strong hover:text-fg-muted",
                        )}
                      >
                        {s}×
                      </button>
                    );
                  })}
                </div>
              </div>
            </div>

            {/* Feasibility warnings */}
            {feasibility === "impossible" && (
              <div className="flex items-start gap-2.5 rounded-md border border-danger/30 bg-danger/5 px-4 py-3">
                <AlertTriangle className="h-4 w-4 text-danger mt-0.5 flex-shrink-0" />
                <p className="text-sm text-danger">
                  Not enough free space to create {localSize - currentSize}{" "}
                  extra {localSize - currentSize === 1 ? "copy" : "copies"} —
                  needs <strong>{fmtBytes(rawNeeded)}</strong>, only{" "}
                  <strong>{fmtBytes(rawFree)}</strong> available.
                </p>
              </div>
            )}
            {feasibility === "tight" && (
              <div className="flex items-start gap-2.5 rounded-md border border-warning/30 bg-warning/5 px-4 py-3">
                <AlertTriangle className="h-4 w-4 text-warning mt-0.5 flex-shrink-0" />
                <p className="text-sm text-warning">
                  Tight fit — only <strong>{fmtBytes(rawFree)}</strong> free,
                  needs <strong>{fmtBytes(rawNeeded)}</strong>. The cluster may
                  hit nearfull warnings during rebalancing.
                </p>
              </div>
            )}

            {/* Capacity + resilience summary */}
            <div className="rounded-md bg-surface-2 border border-border px-5 py-4 space-y-3">
              <div>
                <p className="text-xs text-fg-muted mb-0.5">
                  {showEstimate ? "Estimated capacity" : "Usable capacity"}
                </p>
                <p className="text-2xl font-semibold text-fg tabular-nums">
                  {showEstimate
                    ? `~${fmtBytes(displayCapacity)}`
                    : fmtBytes(displayCapacity)}
                </p>
              </div>
              <div className="h-px bg-border" />
              <div className="space-y-1.5">
                <p
                  className={cn(
                    "text-sm",
                    survivesDisks === 0 ? "text-danger" : "text-fg-muted",
                  )}
                >
                  Survives{" "}
                  <span
                    className={cn(
                      "font-semibold",
                      survivesDisks === 0 ? "text-danger" : "text-fg",
                    )}
                  >
                    {survivesDisks} disk{survivesDisks !== 1 ? "s" : ""}
                  </span>{" "}
                  going down
                </p>
                {localDomain === "host" && (
                  <p
                    className={cn(
                      "text-sm",
                      survivesMachines === 0 ? "text-danger" : "text-fg-muted",
                    )}
                  >
                    Survives{" "}
                    <span
                      className={cn(
                        "font-semibold",
                        survivesMachines === 0 ? "text-danger" : "text-fg",
                      )}
                    >
                      {survivesMachines} machine
                      {survivesMachines !== 1 ? "s" : ""}
                    </span>{" "}
                    going down
                  </p>
                )}
              </div>
            </div>
          </>
        )}

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
          <div className="rounded-md border border-warning/30 bg-warning/5 px-4 py-3 space-y-3">
            <p className="text-sm text-warning font-medium">
              {localMode === "auto"
                ? "Switch to Automatic mode? The system will manage redundancy from now on."
                : `Apply ${localSize}× replication (${localDomain === "osd" ? "disk-level" : "machine-level"})?`}
            </p>
            <p className="text-xs text-fg-muted">
              Changes apply within 60 seconds. Ceph will rebalance data in the
              background.
            </p>
            <div className="flex gap-2">
              <Button
                onClick={() => void applyChanges()}
                disabled={applying}
                size="sm"
                className="gap-2"
              >
                {applying && <RefreshCw className="h-3.5 w-3.5 animate-spin" />}
                {applying ? "Saving…" : "Confirm"}
              </Button>
              <Button
                onClick={() => setConfirm(false)}
                disabled={applying}
                size="sm"
                variant="outline"
              >
                Cancel
              </Button>
            </div>
          </div>
        )}

        {result && (
          <p
            className={cn(
              "text-sm",
              result.startsWith("Saved") ? "text-success" : "text-danger",
            )}
          >
            {result}
          </p>
        )}
      </CardContent>
    </Card>
  );
}

// ── Advanced panel (OSD table + Ceph dashboard) ────────────────────────────────

function OsdTable({
  osds,
  onRefresh,
}: {
  osds: OsdInfo[];
  onRefresh: () => void;
}) {
  const hosts = [...new Set(osds.map((o) => o.host))].sort();

  return (
    <div className="space-y-2">
      <p className="text-xs font-medium text-fg-muted uppercase tracking-wider px-1">
        Disk details
      </p>
      <div className="overflow-x-auto rounded-md border border-border">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-border">
              {[
                "OSD",
                "Host",
                "Class",
                "Size",
                "Fill",
                "Used / Free",
                "PGs",
                "Balance",
                "In/Out",
                "Up/Down",
                "Losable",
                "Removable",
                "",
              ].map((h, i) => (
                <th
                  key={i}
                  className="px-4 py-2.5 text-left text-xs font-medium text-fg-muted whitespace-nowrap first:pl-5 last:pr-5"
                >
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody className="divide-y divide-border/40">
            {hosts.flatMap((host) => {
              const hostOsds = osds.filter((o) => o.host === host);
              return hostOsds.map((osd, idx) => (
                <tr
                  key={osd.id}
                  className="hover:bg-border/20 transition-colors"
                >
                  <td className="pl-5 pr-4 py-3 font-mono text-xs text-primary">
                    {osd.name}
                  </td>
                  <td className="px-4 py-3 text-xs text-fg-muted">
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
                  <td className="px-4 py-3 text-xs text-fg-muted whitespace-nowrap tabular-nums">
                    {osd.size_bytes > 0 ? fmtBytes(osd.size_bytes) : "—"}
                  </td>
                  <td className="px-4 py-3">
                    {osd.size_bytes > 0 ? (
                      <FillBar pct={osd.utilization} />
                    ) : (
                      <span className="text-xs text-border-strong">—</span>
                    )}
                  </td>
                  <td className="px-4 py-3 text-xs text-fg-muted whitespace-nowrap tabular-nums">
                    {osd.size_bytes > 0
                      ? osd.crush_weight > 0
                        ? `${fmtBytes(osd.used_bytes)} / ${fmtBytes(osd.avail_bytes)}`
                        : `${fmtBytes(osd.used_bytes)} / ${fmtBytes(osd.size_bytes)}`
                      : "—"}
                  </td>
                  <td className="px-4 py-3 text-xs text-fg-muted tabular-nums">
                    {osd.pgs}
                  </td>
                  <td className="px-4 py-3">
                    <VarBadge v={osd.var} />
                  </td>
                  <td className="px-4 py-3">
                    <OsdPill on={osd.crush_weight > 0} labels={["In", "Out"]} />
                  </td>
                  <td className="px-4 py-3">
                    <OsdPill on={osd.status === "up"} labels={["Up", "Down"]} />
                  </td>
                  <td className="px-4 py-3">
                    <OsdPill on={osd.ok_to_stop} labels={["Yes", "No"]} />
                  </td>
                  <td className="px-4 py-3">
                    <OsdPill on={osd.safe_to_destroy} labels={["Yes", "No"]} />
                  </td>
                  <td className="pr-5 px-4 py-3">
                    <OsdActions osd={osd} onRefresh={onRefresh} />
                  </td>
                </tr>
              ));
            })}
          </tbody>
        </table>
      </div>
    </div>
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
    <button
      onClick={copy}
      className="text-fg-subtle hover:text-fg-muted transition-colors"
    >
      {copied ? (
        <Check className="h-3.5 w-3.5 text-success" />
      ) : (
        <Copy className="h-3.5 w-3.5" />
      )}
    </button>
  );
}

function AdvancedPanel({
  osds,
  onRefresh,
}: {
  osds: OsdInfo[];
  onRefresh: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [loading, setLoading] = useState(false);
  const [creds, setCreds] = useState<{
    username: string;
    password: string;
  } | null>(null);
  const [showPass, setShowPass] = useState(false);

  function toggle() {
    if (!open && !creds) {
      setLoading(true);
      fetch("/api/ceph/dashboard")
        .then((r) => r.json())
        .then((d) => setCreds(d))
        .catch(() => {})
        .finally(() => setLoading(false));
    }
    setOpen((o) => !o);
  }

  return (
    <div className="rounded-md border border-border overflow-hidden">
      <button
        onClick={toggle}
        className="w-full flex items-center justify-between px-4 py-3 text-sm text-fg-muted hover:text-fg-muted hover:bg-surface-2 transition-colors"
      >
        <span className="font-medium">Advanced</span>
        <ChevronDown
          className={cn("h-4 w-4 transition-transform", open && "rotate-180")}
        />
      </button>

      {open && (
        <div className="border-t border-border px-4 py-4 bg-surface space-y-6">
          {/* OSD table */}
          {osds.length > 0 && <OsdTable osds={osds} onRefresh={onRefresh} />}

          {/* Ceph dashboard */}
          {loading && (
            <p className="text-xs text-fg-subtle">Loading credentials…</p>
          )}
          {creds && (
            <div className="space-y-3">
              <p className="text-xs font-medium text-fg-muted uppercase tracking-wider">
                Ceph dashboard
              </p>
              <div className="flex items-center justify-between">
                <span className="text-xs text-fg-muted w-24">Username</span>
                <div className="flex items-center gap-2 font-mono text-sm text-fg">
                  {creds.username}
                  <CopyButton text={creds.username} />
                </div>
              </div>
              <div className="flex items-center justify-between">
                <span className="text-xs text-fg-muted w-24">Password</span>
                <div className="flex items-center gap-2 font-mono text-sm text-fg">
                  {showPass ? creds.password : "••••••••••••"}
                  <button
                    onClick={() => setShowPass((s) => !s)}
                    className="text-fg-subtle hover:text-fg-muted transition-colors"
                  >
                    {showPass ? (
                      <EyeOff className="h-3.5 w-3.5" />
                    ) : (
                      <Eye className="h-3.5 w-3.5" />
                    )}
                  </button>
                  <CopyButton text={creds.password} />
                </div>
              </div>
              <div className="flex items-center justify-between">
                <span className="text-xs text-fg-muted w-24">Dashboard</span>
                <a
                  href="/ceph-dashboard/"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="flex items-center gap-1.5 text-sm text-primary hover:text-primary transition-colors"
                >
                  Open Ceph dashboard
                  <ExternalLink className="h-3.5 w-3.5" />
                </a>
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Page ──────────────────────────────────────────────────────────────────────

export function StoragePage() {
  const [detail, setDetail] = useState<StorageDetail | null>(null);
  const [policyData, setPolicyData] = useState<StoragePolicyData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  function loadDetail() {
    setLoading(true);
    fetch("/api/ceph/detail")
      .then((r) => r.json())
      .then((d: StorageDetailResponse) => {
        if (d.ok && d.data) {
          setDetail(d.data);
          setError(null);
        } else setError(d.error ?? "Unknown error");
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }

  function loadPolicy() {
    fetch("/api/storage/policy")
      .then((r) => r.json())
      .then((d) => setPolicyData(d as StoragePolicyData))
      .catch(() => {});
  }

  function loadAll() {
    loadDetail();
    loadPolicy();
  }

  useEffect(() => {
    loadAll();
  }, []);

  return (
    <div className="space-y-6 max-w-5xl">
      <div className="flex items-start justify-between">
        <div>
          <p className="text-sm text-fg-muted mt-0.5">
            Disk pool, capacity, and redundancy settings
          </p>
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={loadAll}
          disabled={loading}
          className="gap-2 mt-0.5"
        >
          <RefreshCw className={cn("h-3.5 w-3.5", loading && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {/* Durability posture — always at top if policy data available */}
      {policyData && <DurabilityCard data={policyData} detail={detail} />}

      {error && (
        <div className="flex items-start gap-2 rounded-lg border border-danger/30 bg-danger/5 p-4">
          <AlertTriangle className="h-4 w-4 text-danger mt-0.5 flex-shrink-0" />
          <p className="text-sm text-danger">{error}</p>
        </div>
      )}

      {loading && !detail && (
        <p className="text-sm text-fg-muted">Loading storage data…</p>
      )}

      {detail && (
        <>
          {/* Cluster capacity */}
          <div className="grid grid-cols-3 gap-3">
            {[
              { label: "Raw total", value: fmtBytes(detail.total_bytes) },
              { label: "Raw used", value: fmtBytes(detail.used_bytes) },
              { label: "Raw free", value: fmtBytes(detail.avail_bytes) },
            ].map(({ label, value }) => (
              <Card key={label}>
                <CardContent className="pt-4 pb-3">
                  <p className="text-xs text-fg-muted">{label}</p>
                  <p className="text-lg font-semibold text-fg mt-0.5 tabular-nums">
                    {value}
                  </p>
                </CardContent>
              </Card>
            ))}
          </div>

          <ManageDisksPanel osds={detail.osds} />

          <StorageModePanel
            policyData={policyData}
            osds={detail.osds}
            pools={detail.pools}
            onPolicyChanged={loadPolicy}
          />
        </>
      )}

      <AdvancedPanel osds={detail?.osds ?? []} onRefresh={loadDetail} />
    </div>
  );
}
