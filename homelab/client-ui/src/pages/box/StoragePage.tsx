import { useEffect, useMemo, useState } from "react";
import {
  RefreshCw,
  HardDrive,
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
import { Card } from "@/components/ui/card";
import { Sheet } from "@/components/ui/sheet";
import { Banner, Skeleton } from "@/components/ui/feedback";
import { ForceHealCard } from "@/components/ForceHeal";
import { api } from "@/lib/api";
import { CacheDot } from "@/components/CacheDot";
import { useApi } from "@/lib/useResource";
import { formatBytes } from "@/lib/format";
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

function OfflineDiskBanner({
  detail,
  policy,
}: {
  detail: StorageDetail | undefined;
  policy: StoragePolicyData | undefined;
}) {
  if (!detail) return null;

  const down = detail.osds.filter((o) => o.status !== "up" && o.host !== "");
  const phantom = detail.osds.filter((o) => o.status !== "up" && o.host === "");

  if (down.length === 0 && phantom.length === 0) return null;

  if (down.length === 0) {
    return (
      <Banner
        tone="warning"
        title="A disk did not finish being set up"
        className="mt-2"
      >
        {phantom.length === 1 ? "One disk" : `${phantom.length} disks`} started
        being added and never finished, so nothing is stored on{" "}
        {phantom.length === 1 ? "it" : "them"} yet. Switch the disk off and on
        again to retry. Nothing is at risk —{" "}
        {phantom.length === 1 ? "it" : "they"} never held any of your files.
      </Banner>
    );
  }

  const copies = policy?.target?.size ?? 1;
  const anyUp = detail.osds.some((o) => o.status === "up");

  if (copies <= 1 || !anyUp) {
    return (
      <Banner
        tone="error"
        title="A disk is offline and there is no second copy"
        className="mt-2"
      >
        Whatever was on {down.length === 1 ? "that disk" : "those disks"} is not
        readable right now, and it is not stored anywhere else — so do NOT
        switch it off. There is nothing to rebuild from, and switching it off
        discards it. Get the disk back if you can; otherwise your backups are
        the only copy.
      </Banner>
    );
  }

  return (
    <Banner tone="warning" title="A disk is offline" className="mt-2">
      Your files are still there — they are stored {copies} times, so the other
      copies are serving them. If the disk does not come back, switch it off
      above and YoLab will rebuild the missing copies on the ones that remain.
    </Banner>
  );
}
function fillColor(pct: number): string {
  if (pct >= 85) return "var(--danger)";
  if (pct >= 70) return "var(--warning)";
  return "var(--success)";
}

function FillBar({ pct }: { pct: number }) {
  const color = fillColor(pct);
  return (
    <div className="flex min-w-[120px] items-center gap-2">
      <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-surface-3">
        <div
          className="h-full rounded-full transition-all"
          style={{ width: `${Math.min(pct, 100)}%`, background: color }}
        />
      </div>
      <span className="w-10 text-right text-xs tabular-nums" style={{ color }}>
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
        "font-mono text-xs tabular-nums",
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

type DiskState =
  | "active"
  | "pending"
  | "missing"
  | "draining"
  | "excluded"
  | "historical"
  | "foreign"
  | "unidentified"
  | "failing"
  | "blocked"
  | "stale"
  | "removing"
  | "removable";

function diskState(disk: DiskInfo): DiskState {
  if (disk.ownership === "unknown") return "unidentified";
  if (disk.foreign_ceph) return "foreign";

  const on = disk.desired === "ON";
  if (on && !disk.connected) return "missing";
  if (!on && !disk.connected) return "historical";

  switch (disk.phase) {
    case "active":
      return "active";
    case "creating":
      return "pending";
    case "retrying":
      return "failing";
    case "blocked":
      return "blocked";
    case "draining":
      return "draining";
    case "removing":
      return "removing";
    case "removable":
      return "removable";
    case "unknown":
      return "stale";
  }

  if (on && disk.is_our_osd) return "active";
  if (on) return "pending";
  if (disk.is_our_osd) return "draining";
  return "excluded";
}

const STATE_META: Record<
  DiskState,
  { label: string; color: string; dot: string; pulse?: boolean }
> = {
  active: { label: "In use", color: "text-fg-muted", dot: "bg-success" },
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
    label: "Being removed — moving data off",
    color: "text-warning",
    dot: "bg-warning",
    pulse: true,
  },
  failing: {
    label: "Could not be added",
    color: "text-danger",
    dot: "bg-danger",
  },
  blocked: {
    label: "Needs a decision",
    color: "text-warning",
    dot: "bg-warning",
  },
  removing: {
    label: "Finishing up — do not unplug yet",
    color: "text-warning",
    dot: "bg-warning",
    pulse: true,
  },
  removable: {
    label: "Safe to unplug",
    color: "text-success",
    dot: "bg-success",
  },
  stale: {
    label: "Checking…",
    color: "text-fg-muted",
    dot: "bg-fg-subtle",
    pulse: true,
  },
  excluded: {
    label: "Connected, not in use",
    color: "text-fg-muted",
    dot: "bg-fg-subtle",
  },
  historical: {
    label: "Not connected",
    color: "text-fg-subtle",
    dot: "bg-border-strong",
  },
  foreign: {
    label: "Has data from another system",
    color: "text-warning",
    dot: "bg-warning",
  },
  unidentified: {
    label: "Can't identify this disk yet",
    color: "text-fg-muted",
    dot: "bg-fg-subtle",
    pulse: true,
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
  const [err, setErr] = useState<string | null>(null);

  const state = diskState(disk);
  const sm = STATE_META[state];
  const isOn = disk.desired === "ON";

  async function toggle() {
    const next = isOn ? "OFF" : "ON";
    const needsConfirm =
      (next === "OFF" && disk.is_our_osd) ||
      (next === "ON" && state === "foreign");
    if (needsConfirm && !confirm) {
      setConfirm(true);
      return;
    }
    setBusy(true);
    setErr(null);
    setConfirm(false);
    try {
      const d = await api.put<{ ok?: boolean; error?: string }>(
        `/api/disks/${node}/${disk.id}`,
        { desired: next },
      );
      if (!d.ok) setErr(d.error ?? "Unknown error");
      else onChanged();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  const label = disk.model || disk.device || disk.id;
  const isMissing = state === "missing";

  return (
    <div
      className={cn(
        "flex items-center gap-4 border-b border-border px-5 py-4 last:border-0",
        isMissing && "bg-danger-soft",
      )}
    >
      <div className="shrink-0">
        {state === "missing" ? (
          <WifiOff className="h-5 w-5 text-danger" strokeWidth={1.5} />
        ) : disk.is_loop ? (
          <Cpu className="h-5 w-5 text-fg-muted" strokeWidth={1.5} />
        ) : (
          <HardDrive className="h-5 w-5 text-fg-muted" strokeWidth={1.5} />
        )}
      </div>

      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-fg">
          {label} {}
          {disk.connected && disk.size_bytes > 0 && (
            <span className="ml-2 font-normal text-fg-muted">
              {formatBytes(disk.size_bytes)}
            </span>
          )}
        </p>
        <div className="mt-1 flex items-center gap-1.5">
          <span
            className={cn(
              "inline-block h-1.5 w-1.5 shrink-0 rounded-full",
              sm.dot,
              sm.pulse && "animate-pulse",
            )}
          />
          <p className={cn("text-sm", sm.color)}>
            {sm.label}
            {disk.is_loop && (
              <span className="text-fg-subtle"> · built into this machine</span>
            )}
          </p>
        </div>
        {}
        {disk.message && (
          <p className="mt-1 text-sm text-fg-muted">{disk.message}</p>
        )}
        {err && <p className="mt-1 text-sm text-danger">{err}</p>}
      </div>

      {osd && state === "active" && (
        <div className="hidden shrink-0 flex-col items-end gap-1 sm:flex">
          <FillBar pct={osd.utilization} />
        </div>
      )}

      {confirm && (
        <div className="flex shrink-0 items-center gap-2">
          <span className="hidden text-sm text-fg-muted sm:inline">
            {isOn
              ? "Data moves off first."
              : "Everything on it will be erased."}
          </span>
          <Button
            size="sm"
            variant="danger"
            disabled={busy}
            onClick={() => void toggle()}
          >
            {isOn ? "Remove" : "Erase and use"}
          </Button>
          <Button size="sm" variant="ghost" onClick={() => setConfirm(false)}>
            Cancel
          </Button>
        </div>
      )}

      {}
      {state !== "unidentified" && !confirm && (
        <button
          onClick={() => void toggle()}
          disabled={busy}
          role="switch"
          aria-checked={isOn}
          aria-label={isOn ? `Stop using ${label}` : `Use ${label}`}
          className={cn(
            "relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors",
            isOn ? "bg-primary" : "bg-surface-3",
            busy && "cursor-not-allowed opacity-50",
          )}
        >
          <span
            className={cn(
              "inline-block h-6 w-6 rounded-full bg-white shadow transition-transform",
              isOn ? "translate-x-[1.375rem]" : "translate-x-0.5",
            )}
          />
        </button>
      )}
    </div>
  );
}

function DiskList({
  disks,
  osds,
  loading,
  onChanged,
}: {
  disks: Record<string, DiskInfo[]> | undefined;
  osds: OsdInfo[];
  loading: boolean;
  onChanged: () => void;
}) {
  const [showPast, setShowPast] = useState(false);
  const nodes = disks ? Object.keys(disks).sort() : [];

  function osdFor(disk: DiskInfo) {
    return disk.osd_id === null
      ? undefined
      : osds.find((o) => o.id === disk.osd_id);
  }

  const present: [string, DiskInfo][] = [];
  const past: [string, DiskInfo][] = [];
  for (const node of nodes) {
    for (const disk of disks?.[node] ?? []) {
      (diskState(disk) === "historical" ? past : present).push([node, disk]);
    }
  }

  if (loading && !disks) {
    return (
      <Card className="divide-y divide-border p-0">
        {[0, 1].map((i) => (
          <div key={i} className="flex items-center gap-4 px-5 py-4">
            <Skeleton className="h-5 w-5 rounded-lg" />
            <div className="flex-1 space-y-2">
              <Skeleton className="h-4 w-40" />
              <Skeleton className="h-3 w-24" />
            </div>
          </div>
        ))}
      </Card>
    );
  }

  if (present.length === 0 && past.length === 0) {
    return (
      <Card className="p-5">
        <p className="text-sm text-fg-muted">
          No disks found yet. Plug one in and it will appear here.
        </p>
      </Card>
    );
  }

  const multiNode = nodes.length > 1;

  return (
    <>
      <Card className="divide-y divide-border p-0">
        {present.map(([node, disk]) => (
          <div key={`${node}/${disk.id}`}>
            {multiNode && (
              <p className="bg-surface-2 px-5 py-1.5 text-xs font-medium text-fg-muted">
                {node}
              </p>
            )}
            <DiskRow
              node={node}
              disk={disk}
              osd={osdFor(disk)}
              onChanged={onChanged}
            />
          </div>
        ))}
        {present.length === 0 && (
          <p className="px-5 py-4 text-sm text-fg-muted">
            None of the disks this machine has seen are connected right now.
          </p>
        )}
      </Card>

      {past.length > 0 && (
        <div className="mt-3">
          <button
            onClick={() => setShowPast((s) => !s)}
            className="flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
            aria-expanded={showPast}
          >
            {past.length} disk{past.length === 1 ? "" : "s"} seen before but not
            connected
            <ChevronDown
              className={cn(
                "h-4 w-4 transition-transform",
                showPast && "rotate-180",
              )}
            />
          </button>
          {showPast && (
            <Card className="mt-2 divide-y divide-border p-0 opacity-70">
              {past.map(([node, disk]) => (
                <DiskRow
                  key={`${node}/${disk.id}`}
                  node={node}
                  disk={disk}
                  osd={osdFor(disk)}
                  onChanged={onChanged}
                />
              ))}
            </Card>
          )}
        </div>
      )}
    </>
  );
}

function safetyLine(
  data: StoragePolicyData | undefined,
  detail: StorageDetail | undefined,
): { tone: "ok" | "warn" | "bad"; text: string } | null {
  if (!data) return null;
  const { target } = data;
  if (!target) return null;
  const offline = detail?.osds.filter((o) => o.status !== "up").length ?? 0;
  const survives = target.size - 1;
  const unit = target.failure_domain === "host" ? "machine" : "disk";

  const suffix =
    offline > 0
      ? ` ${offline} ${offline === 1 ? "disk is" : "disks are"} offline right now.`
      : "";

  if (target.size <= 1) {
    return {
      tone: "bad",
      text: `Your files are stored once. If a ${unit} fails, what was on it is gone — backups are your only copy.${suffix}`,
    };
  }
  return {
    tone: offline > 0 ? "warn" : "ok",
    text: `Any ${survives === 1 ? "one" : survives} ${unit}${survives === 1 ? "" : "s"} can fail without losing anything.${suffix}`,
  };
}

function CapacityCard({
  detail,
  policy,
  loading,
}: {
  detail: StorageDetail | undefined;
  policy: StoragePolicyData | undefined;
  loading: boolean;
}) {
  const pools = (detail?.pools ?? []).filter(
    (p) => !p.name.startsWith(".") && p.name !== "images",
  );
  const used = pools.reduce((s, p) => s + p.stored_bytes, 0);

  const poolFree = pools.length
    ? Math.min(...pools.map((p) => p.max_avail_bytes))
    : 0;

  const rawFree = detail?.avail_bytes ?? poolFree;
  const free = Math.min(poolFree, rawFree);
  const total = used + free;
  const pct = total > 0 ? (used / total) * 100 : 0;

  const safety = safetyLine(policy, detail);

  if (loading && !detail) {
    return (
      <Card className="space-y-4 p-6">
        <Skeleton className="h-9 w-64" />
        <Skeleton className="h-2 w-full" />
        <Skeleton className="h-4 w-80" />
      </Card>
    );
  }

  return (
    <Card className="p-6">
      {total > 0 ? (
        <>
          <p className="font-display text-3xl text-fg">
            {formatBytes(free)} <span className="text-fg-muted">free</span>
          </p>
          <p className="mt-1 text-sm text-fg-muted">
            {formatBytes(used)} of {formatBytes(total)} used
          </p>
          <div className="mt-4 h-2 overflow-hidden rounded-full bg-surface-3">
            <div
              className="h-full rounded-full transition-all"
              style={{
                width: `${Math.max(Math.min(pct, 100), used > 0 ? 1.5 : 0)}%`,
                background: fillColor(pct),
              }}
            />
          </div>
        </>
      ) : (
        <p className="text-sm text-fg-muted">
          Storage is still being set up. This fills in once the first disk is
          ready.
        </p>
      )}

      {safety && (
        <p
          className={cn(
            "mt-5 border-t border-border pt-4 text-sm",
            safety.tone === "bad"
              ? "text-danger"
              : safety.tone === "warn"
                ? "text-warning"
                : "text-fg-muted",
          )}
        >
          {safety.text}
        </p>
      )}
    </Card>
  );
}

type Domain = "osd" | "host";

function estimateUsable(
  osds: OsdInfo[],
  size: number,
  domain: "osd" | "host",
): number {
  const SAFETY = 0.95;
  const totalRaw = osds.reduce((s, o) => s + o.size_bytes, 0);
  if (totalRaw === 0) return 0;

  const buckets = placementBuckets(osds, domain);
  if (buckets.length === 0) return 0;

  const effective = Math.min(size, buckets.length);

  if (effective === buckets.length) {
    return Math.min(...buckets) * SAFETY;
  }
  return (totalRaw / effective) * SAFETY;
}

function placementBuckets(osds: OsdInfo[], domain: "osd" | "host"): number[] {
  if (domain === "osd") return osds.map((o) => o.size_bytes);
  const hostMap = new Map<string, number>();
  for (const o of osds) {
    hostMap.set(o.host, (hostMap.get(o.host) ?? 0) + o.size_bytes);
  }
  return [...hostMap.values()];
}

function RedundancySheet({
  open,
  onClose,
  policyData,
  osds,
  pools,
  onPolicyChanged,
}: {
  open: boolean;
  onClose: () => void;
  policyData: StoragePolicyData;
  osds: OsdInfo[];
  pools: PoolInfo[];
  onPolicyChanged: () => void;
}) {
  const saved = policyData.policy ?? {
    size: 1,
    failure_domain: "osd" as const,
  };

  const [size, setSize] = useState<number>(saved.size);
  const [domain, setDomain] = useState<Domain>(saved.failure_domain as Domain);
  const [applying, setApplying] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [result, setResult] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setSize(saved.size);
    setDomain(saved.failure_domain as Domain);
    setConfirm(false);
    setResult(null);
  }, [open, saved.size, saved.failure_domain]);

  const nDisks = osds.length;
  const nNodes = new Set(osds.map((o) => o.host)).size;

  const changed = size !== saved.size || domain !== saved.failure_domain;

  const cephFs = pools.filter((p) => !p.name.startsWith("."));
  const totalStored = cephFs.reduce((s, p) => s + p.stored_bytes, 0);
  const rawFree = osds.reduce((s, o) => s + o.avail_bytes, 0);
  const places = domain === "osd" ? nDisks : nNodes;
  const effNew = Math.min(size, places);
  const effOld = Math.min(saved.size, places);
  const rawNeeded = (effNew - effOld) * totalStored;
  let feasibility: "ok" | "tight" | "impossible" | null = null;
  if (rawNeeded > 0) {
    if (rawFree < rawNeeded) feasibility = "impossible";
    else if (rawFree < rawNeeded * 1.3) feasibility = "tight";
    else feasibility = "ok";
  }

  const authoritative = cephFs[0]?.max_avail_bytes ?? 0;
  const showEstimate = changed || authoritative === 0;
  const capacity = showEstimate
    ? estimateUsable(osds, size, domain)
    : authoritative;

  async function apply() {
    setApplying(true);
    setResult(null);
    try {
      const body = { size, failure_domain: domain };
      const d = await api.put<{ ok?: boolean; error?: string }>(
        "/api/storage/policy",
        body,
      );
      if (d.ok) {
        onPolicyChanged();
        onClose();
      } else {
        setResult(d.error ?? "Unknown error");
      }
    } catch (e) {
      setResult(e instanceof Error ? e.message : String(e));
    } finally {
      setApplying(false);
    }
  }

  const Choice = ({
    active,
    onClick,
    title,
    body,
    disabled,
  }: {
    active: boolean;
    onClick: () => void;
    title: string;
    body?: string;
    disabled?: boolean;
  }) => (
    <button
      onClick={onClick}
      disabled={disabled}
      className={cn(
        "flex-1 rounded-xl border px-4 py-3 text-left transition-colors",
        disabled
          ? "cursor-not-allowed border-border text-fg-subtle opacity-50"
          : active
            ? "border-primary bg-primary-soft text-primary"
            : "border-border text-fg hover:border-border-strong",
      )}
    >
      <span className="block text-sm font-medium">{title}</span>
      {body && <span className="mt-0.5 block text-xs opacity-80">{body}</span>}
    </button>
  );

  return (
    <Sheet
      open={open}
      onClose={onClose}
      wide
      title="How safe should your files be?"
      subtitle="More copies survive more failures, and leave less room."
      footer={
        confirm ? (
          <div className="flex flex-col gap-2 sm:flex-row sm:justify-end">
            <Button
              variant="secondary"
              onClick={() => setConfirm(false)}
              disabled={applying}
            >
              Back
            </Button>
            <Button onClick={() => void apply()} loading={applying}>
              Yes, change it
            </Button>
          </div>
        ) : (
          <Button
            full
            disabled={!changed || feasibility === "impossible"}
            onClick={() => setConfirm(true)}
          >
            {changed ? "Save changes" : "No changes"}
          </Button>
        )
      }
    >
      {confirm ? (
        <div className="space-y-3">
          <p className="text-sm text-fg">
            {`Keep ${size} ${size === 1 ? "copy" : "copies"} of everything, spread across ${domain === "osd" ? "different disks" : "different machines"}?`}
          </p>
          <p className="text-sm text-fg-muted">
            Your files stay available while this happens. Moving them around in
            the background can take a while on a large library.
          </p>
          {result && <p className="text-sm text-danger">{result}</p>}
        </div>
      ) : (
        <div className="space-y-6">
          <>
            <div>
              <p className="mb-2 text-sm font-medium text-fg">
                Spread copies across
              </p>
              <div className="flex gap-2">
                <Choice
                  active={domain === "osd"}
                  onClick={() => setDomain("osd")}
                  title="Different disks"
                  body={`${nDisks} available`}
                />
                <Choice
                  active={domain === "host"}
                  onClick={() => setDomain("host")}
                  title="Different machines"
                  body={`${nNodes} available`}
                />
              </div>
            </div>

            <div>
              <p className="mb-2 text-sm font-medium text-fg">
                How many copies
              </p>
              {}
              <div className="flex items-center gap-4">
                <Button
                  size="sm"
                  variant="outline"
                  disabled={size <= 1}
                  onClick={() => setSize((n) => Math.max(1, n - 1))}
                  aria-label="One fewer copy"
                >
                  −
                </Button>
                <span
                  className="w-12 text-center font-display text-3xl tabular-nums text-fg"
                  aria-live="polite"
                >
                  {size}
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => setSize((n) => n + 1)}
                  aria-label="One more copy"
                >
                  +
                </Button>
              </div>

              <div className="mt-3">
                {size === 1 ? (
                  <Banner tone="error" title="No protection">
                    One copy means if a {domain === "osd" ? "disk" : "machine"}{" "}
                    fails, whatever was on it is gone. Your backups would be the
                    only copy left.
                  </Banner>
                ) : size > places ? (
                  <Banner
                    tone="warning"
                    title={`Only ${places} of those ${size} copies fit right now`}
                  >
                    You have {places} {domain === "osd" ? "disk" : "machine"}
                    {places === 1 ? "" : "s"}, and each copy needs its own.
                    YoLab will keep {places} for now and make the rest
                    automatically when you add{" "}
                    {domain === "osd" ? "a disk" : "a machine"}. Nothing is at
                    risk in the meantime.
                  </Banner>
                ) : (
                  <p className="text-sm text-fg-muted">
                    Survives {size - 1} {domain === "osd" ? "disk" : "machine"}
                    {size - 1 === 1 ? "" : "s"} failing.
                  </p>
                )}
              </div>
            </div>

            {feasibility === "impossible" && (
              <Banner tone="error" title="Not enough room for that">
                Making {size - saved.size} more{" "}
                {size - saved.size === 1 ? "copy" : "copies"} needs{" "}
                {formatBytes(rawNeeded)} and only {formatBytes(rawFree)} is
                free. Add a disk first.
              </Banner>
            )}
            {feasibility === "tight" && (
              <Banner tone="warning" title="This will be a tight fit">
                It needs {formatBytes(rawNeeded)} and {formatBytes(rawFree)} is
                free. It should work, but you will be close to full while the
                copies are made.
              </Banner>
            )}

            <div className="rounded-xl bg-surface-2 p-4">
              <p className="text-sm text-fg-muted">
                Room for your files with this setting
              </p>
              <p className="mt-0.5 font-display text-2xl text-fg">
                {showEstimate ? "about " : ""}
                {formatBytes(capacity)}
              </p>
            </div>
          </>

          {result && <p className="text-sm text-danger">{result}</p>}
        </div>
      )}
    </Sheet>
  );
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 2000);
        });
      }}
      className="text-fg-subtle transition-colors hover:text-fg"
      aria-label="Copy"
    >
      {copied ? (
        <Check className="h-4 w-4 text-success" />
      ) : (
        <Copy className="h-4 w-4" />
      )}
    </button>
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
      const d = await api.post<{ ok?: boolean; error?: string }>(path);
      if (!d.ok) setErr(d.error ?? "Unknown error");
      else {
        setConfirm(false);
        onRefresh();
      }
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
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

function OsdTable({
  osds,
  onRefresh,
}: {
  osds: OsdInfo[];
  onRefresh: () => void;
}) {
  const hosts = [...new Set(osds.map((o) => o.host))].sort();

  return (
    <div className="overflow-x-auto rounded-xl border border-border">
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
                className="whitespace-nowrap px-4 py-2.5 text-left text-xs font-medium text-fg-muted first:pl-5 last:pr-5"
              >
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="divide-y divide-border">
          {hosts.flatMap((host) => {
            const hostOsds = osds.filter((o) => o.host === host);
            return hostOsds.map((osd, idx) => (
              <tr key={osd.id}>
                <td className="py-3 pl-5 pr-4 font-mono text-xs text-primary">
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
                <td className="whitespace-nowrap px-4 py-3 text-xs tabular-nums text-fg-muted">
                  {osd.size_bytes > 0 ? fmtBytes(osd.size_bytes) : "—"}
                </td>
                <td className="px-4 py-3">
                  {osd.size_bytes > 0 ? (
                    <FillBar pct={osd.utilization} />
                  ) : (
                    <span className="text-xs text-fg-subtle">—</span>
                  )}
                </td>
                <td className="whitespace-nowrap px-4 py-3 text-xs tabular-nums text-fg-muted">
                  {osd.size_bytes > 0
                    ? osd.crush_weight > 0
                      ? `${fmtBytes(osd.used_bytes)} / ${fmtBytes(osd.avail_bytes)}`
                      : `${fmtBytes(osd.used_bytes)} / ${fmtBytes(osd.size_bytes)}`
                    : "—"}
                </td>
                <td className="px-4 py-3 text-xs tabular-nums text-fg-muted">
                  {osd.pgs}
                </td>
                <td className="px-4 py-3">
                  <VarBadge v={osd.var} />
                </td>
                <td className="px-4 py-3">
                  {}
                  <OsdPill on={osd.reweight > 0.5} labels={["In", "Out"]} />
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
                <td className="px-4 py-3 pr-5">
                  <OsdActions osd={osd} onRefresh={onRefresh} />
                </td>
              </tr>
            ));
          })}
        </tbody>
      </table>
    </div>
  );
}

function AdvancedPanel({
  detail,
  onRefresh,
  refreshing,
}: {
  detail: StorageDetail | undefined;
  onRefresh: () => void;
  refreshing: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [creds, setCreds] = useState<{
    username: string;
    password: string;
  } | null>(null);
  const [showPass, setShowPass] = useState(false);

  function toggle() {
    if (!open && !creds) {
      void api
        .get<{ username: string; password: string }>("/api/ceph/dashboard")
        .then(setCreds)
        .catch(() => {});
    }
    setOpen((o) => !o);
  }

  const osds = detail?.osds ?? [];

  return (
    <div className="mt-8">
      <button
        onClick={toggle}
        className="flex items-center gap-1.5 text-sm text-fg-muted hover:text-fg"
        aria-expanded={open}
      >
        Technical details
        <ChevronDown
          className={cn("h-4 w-4 transition-transform", open && "rotate-180")}
        />
      </button>

      {open && (
        <div className="mt-4 space-y-6">
          {}
          <div className="flex items-center justify-end">
            {}
            <Button
              size="sm"
              variant="ghost"
              onClick={onRefresh}
              disabled={refreshing}
            >
              <RefreshCw
                className={cn("h-4 w-4", refreshing && "animate-spin")}
              />
              Refresh
            </Button>
          </div>

          {}
          {osds.length > 0 && <OsdTable osds={osds} onRefresh={onRefresh} />}

          {creds && (
            <div className="space-y-3 rounded-xl border border-border p-4">
              <p className="text-sm font-medium text-fg">Ceph dashboard</p>
              <div className="flex items-center justify-between">
                <span className="text-sm text-fg-muted">Username</span>
                <span className="flex items-center gap-2 font-mono text-sm text-fg">
                  {creds.username}
                  <CopyButton text={creds.username} />
                </span>
              </div>
              <div className="flex items-center justify-between">
                <span className="text-sm text-fg-muted">Password</span>
                <span className="flex items-center gap-2 font-mono text-sm text-fg">
                  {showPass ? creds.password : "••••••••••••"}
                  <button
                    onClick={() => setShowPass((s) => !s)}
                    className="text-fg-subtle hover:text-fg"
                    aria-label={showPass ? "Hide password" : "Show password"}
                  >
                    {showPass ? (
                      <EyeOff className="h-4 w-4" />
                    ) : (
                      <Eye className="h-4 w-4" />
                    )}
                  </button>
                  <CopyButton text={creds.password} />
                </span>
              </div>
              <a
                href="/ceph-dashboard/"
                target="_blank"
                rel="noopener noreferrer"
                className="inline-flex items-center gap-1.5 text-sm text-primary"
              >
                Open Ceph dashboard
                <ExternalLink className="h-3.5 w-3.5" />
              </a>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function StoragePage() {
  const [editing, setEditing] = useState(false);

  const detailRes = useApi<StorageDetailResponse>(
    "storage-detail",
    "/api/ceph/detail",
    { pollMs: 20_000 },
  );
  const policyRes = useApi<StoragePolicyData>(
    "storage-policy",
    "/api/storage/policy",
  );
  const disksRes = useApi<Record<string, DiskInfo[]>>(
    "storage-disks",
    "/api/disks",
    { pollMs: 20_000 },
  );

  const detail = detailRes.data?.ok ? detailRes.data.data : undefined;
  const cephError =
    detailRes.data && !detailRes.data.ok
      ? (detailRes.data.error ?? "Storage is not responding.")
      : null;

  function refreshAll() {
    void detailRes.refresh();
    void policyRes.refresh();
    void disksRes.refresh();
  }

  const policy = policyRes.data;
  const summary = useMemo(() => {
    if (!policy) return null;
    const { target } = policy;
    if (!target) return null;
    const unit = target.failure_domain === "host" ? "machines" : "disks";
    return `${target.size} ${target.size === 1 ? "copy" : "copies"} across different ${unit}`;
  }, [policy]);

  return (
    <div className="space-y-6">
      <ForceHealCard />

      {cephError && (
        <Banner tone="error" title="Storage is not responding">
          {cephError}
        </Banner>
      )}

      {}
      <div className="flex items-center justify-end -mb-3">
        <CacheDot cache={detailRes.cache} />
      </div>
      <CapacityCard
        detail={detail}
        policy={policy}
        loading={detailRes.loading}
      />

      <section>
        <h2 className="mb-3 flex items-center gap-2 text-sm font-semibold text-fg-muted">
          Disks
          <CacheDot cache={disksRes.cache} />
        </h2>
        <DiskList
          disks={disksRes.data}
          osds={detail?.osds ?? []}
          loading={disksRes.loading}
          onChanged={refreshAll}
        />
        <p className="mt-3 text-sm text-fg-subtle">
          A new disk does nothing until you switch it on. Switching one off
          moves its data elsewhere first.
        </p>
      </section>

      {policy && (
        <section>
          <h2 className="mb-3 text-sm font-semibold text-fg-muted">
            Protection
          </h2>
          <Card className="flex items-center gap-4 p-5">
            <div className="min-w-0 flex-1">
              <p className="text-sm text-fg">{summary}</p>
            </div>
            <Button
              size="sm"
              variant="secondary"
              onClick={() => setEditing(true)}
            >
              Change
            </Button>
          </Card>
        </section>
      )}

      {policy && (
        <RedundancySheet
          open={editing}
          onClose={() => setEditing(false)}
          policyData={policy}
          osds={detail?.osds ?? []}
          pools={detail?.pools ?? []}
          onPolicyChanged={refreshAll}
        />
      )}

      <AdvancedPanel
        detail={detail}
        onRefresh={refreshAll}
        refreshing={detailRes.loading}
      />

      {}
      <OfflineDiskBanner detail={detail} policy={policyRes.data} />
    </div>
  );
}
