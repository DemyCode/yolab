import { useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import {
  ChevronRight,
  Cpu,
  ExternalLink,
  Eye,
  EyeOff,
  HardDrive,
  RefreshCw,
  WifiOff,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Select, Switch } from "@/components/ui/input";
import { Banner, Skeleton } from "@/components/ui/feedback";
import {
  CopyButton,
  DisclosureRow,
  IconButton,
  Row,
  Section,
  ValueRow,
} from "@/components/ui/list";
import { AppIcon } from "@/components/AppIcon";
import { ForceHealCard } from "@/components/ForceHeal";
import { DataMovementCard, DrainProgress } from "@/components/DataMovement";
import {
  AnimatedList,
  Collapse,
  RollingNumber,
  Swap,
} from "@/components/motion";
import { api } from "@/lib/api";
import { useApi, useResource } from "@/lib/useResource";
import { formatBytes } from "@/lib/format";
import {
  isVisible,
  movementCopy,
  needsAttentionEverywhere,
  useMovement,
} from "@/lib/movement";
import {
  POLL_IDLE_MS,
  POLL_MOVING_MS,
  POLL_USAGE_MS,
  diskState,
  estimateChange,
  fillTone,
  formatCephBytes,
  pickBanner,
  placesFor,
  protectionLine,
  rawPercent,
  statusLine,
  usageRows,
  type DiskState,
  type Domain,
  type Tone,
} from "@/lib/storage";
import { cn } from "@/lib/utils";
import type { AppInfo, CatalogApp } from "@/types/apps";
import type {
  DiskInfo,
  Movement,
  Osd,
  OsdChecks,
  StorageOverview,
  StoragePolicyData,
  StorageUsage,
} from "@/types/storage";

const TONE_TEXT: Record<Tone, string> = {
  ok: "text-fg-muted",
  warn: "text-warning",
  bad: "text-danger",
};

const TONE_FILL: Record<Tone, string> = {
  ok: "bg-success",
  warn: "bg-warning",
  bad: "bg-danger",
};

const TONE_DOT: Record<Tone, string> = {
  ok: "bg-success",
  warn: "bg-warning",
  bad: "bg-danger",
};

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono tabular-nums">{children}</span>;
}

function Bytes({ value }: { value: number }) {
  return (
    <Mono>
      <RollingNumber value={value} format={formatCephBytes} />
    </Mono>
  );
}

function Bar({ pct, tone }: { pct: number; tone: Tone }) {
  return (
    <div className="h-2 overflow-hidden rounded-full bg-surface-3">
      <div
        className={cn(
          "h-full rounded-full transition-[width] duration-700",
          TONE_FILL[tone],
        )}
        style={{ width: `${Math.max(Math.min(pct, 100), pct > 0 ? 1.5 : 0)}%` }}
      />
    </div>
  );
}

function storedTimes(copies: number): string {
  return copies <= 1
    ? "Everything is stored once"
    : `Everything is stored ${copies} times`;
}

function StatusLine({
  overview,
  policy,
  movement,
}: {
  overview: StorageOverview | undefined;
  policy: StoragePolicyData | undefined;
  movement: Movement | undefined;
}) {
  const happening = isVisible(movement)
    ? (movementCopy(movement)?.headline ?? null)
    : null;
  const line = statusLine(overview, policy?.target, happening);
  if (!line) return <Skeleton className="-mt-4 h-4 w-64" />;
  return (
    <p className="-mt-4 flex items-center gap-2 text-sm text-fg-muted">
      <span
        aria-hidden
        className={cn("h-2 w-2 shrink-0 rounded-full", TONE_DOT[line.tone])}
      />
      <Swap id={line.text}>{line.text}</Swap>
    </p>
  );
}

function SpaceSection({
  overview,
  loading,
}: {
  overview: StorageOverview | undefined;
  loading: boolean;
}) {
  const space = overview?.space;
  return (
    <Section title="Space">
      {loading && !overview ? (
        <div className="space-y-4 px-5 py-5">
          <Skeleton className="h-9 w-64" />
          <Skeleton className="h-2 w-full" />
          <Skeleton className="h-4 w-80" />
        </div>
      ) : !overview || !space ? (
        <Row
          label="Storage is still being set up"
          detail="This fills in once the first disk is ready."
        />
      ) : (
        <div className="px-5 py-5">
          <p className="font-display text-3xl text-fg">
            <Bytes value={space.free_bytes} />{" "}
            <span className="text-fg-muted">free for your files</span>
          </p>
          <div className="mt-4">
            <Bar
              pct={rawPercent(overview.raw)}
              tone={fillTone(rawPercent(overview.raw))}
            />
          </div>
          <p className="mt-3 text-sm text-fg-muted">
            Apps’ files <Bytes value={space.apps_bytes} /> · App programs{" "}
            <Bytes value={space.images_bytes} />
            {space.other_bytes > 0 && (
              <>
                {" "}
                · Other <Bytes value={space.other_bytes} />
              </>
            )}
          </p>
          <p className="mt-1 text-sm text-fg-muted">
            {storedTimes(space.copies)}, so the disks hold{" "}
            <Bytes value={overview.raw.used_bytes} /> of{" "}
            <Bytes value={overview.raw.total_bytes} />.
          </p>
          {fillTone(space.fullest_disk_percent) !== "ok" && (
            <p
              className={cn(
                "mt-1 text-sm",
                TONE_TEXT[fillTone(space.fullest_disk_percent)],
              )}
            >
              Your fullest disk is{" "}
              <Mono>{Math.round(space.fullest_disk_percent)}%</Mono> full.
              Storage stops accepting new files when any one disk fills up, so
              add a disk or remove files before then.
            </p>
          )}
        </div>
      )}
    </Section>
  );
}

function UsageSection({ imagesBytes }: { imagesBytes: number }) {
  const usage = useApi<StorageUsage>("storage-usage", "/api/storage/usage", {
    pollMs: POLL_USAGE_MS,
  });
  const apps = useApi<AppInfo[]>("apps", "/api/apps");
  const catalog = useApi<CatalogApp[]>("catalog", "/api/apps/catalog");
  const rows = usageRows(
    usage.data,
    apps.data ?? [],
    catalog.data ?? [],
    imagesBytes,
  );
  const largest = rows[0]?.bytes ?? 0;

  return (
    <>
      <Section title="What’s using space">
        {usage.loading && !usage.data ? (
          [0, 1, 2].map((i) => (
            <div key={i} className="flex items-center gap-3 px-5 py-4">
              <Skeleton className="h-8 w-8" />
              <Skeleton className="h-4 flex-1" />
            </div>
          ))
        ) : usage.error && !usage.data ? (
          <Row
            label="App sizes could not be read right now"
            detail={usage.error}
          />
        ) : rows.length === 0 ? (
          <Row label="No app is storing anything yet" />
        ) : (
          <AnimatedList items={rows} keyOf={(r) => r.key}>
            {(r) => {
              const body = (
                <>
                  <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-control bg-surface-2">
                    {r.instance ? (
                      <AppIcon
                        appId={r.appId}
                        icon={r.icon}
                        name={r.label}
                        className="h-5 w-5"
                      />
                    ) : (
                      <HardDrive className="h-4 w-4 text-fg-muted" />
                    )}
                  </div>
                  <div className="min-w-0 flex-1">
                    <div className="flex items-baseline justify-between gap-3">
                      <span className="truncate text-sm font-medium text-fg">
                        {r.label}
                      </span>
                      <span className="shrink-0 text-sm text-fg-muted">
                        <Bytes value={r.bytes} />
                      </span>
                    </div>
                    <div className="mt-2 h-1 overflow-hidden rounded-full bg-surface-3">
                      <div
                        className="h-full rounded-full bg-fg-subtle transition-[width] duration-700"
                        style={{
                          width: `${largest > 0 ? Math.max((r.bytes / largest) * 100, 1) : 0}%`,
                        }}
                      />
                    </div>
                  </div>
                </>
              );
              return r.instance ? (
                <Link
                  to={`/app/${r.instance}`}
                  className="flex items-center gap-3 px-5 py-4 transition-colors hover:bg-surface-2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-primary"
                >
                  {body}
                  <ChevronRight className="h-5 w-5 shrink-0 text-fg-subtle" />
                </Link>
              ) : (
                <div className="flex items-center gap-3 px-5 py-4">
                  {body}
                  <span aria-hidden className="w-5 shrink-0" />
                </div>
              );
            }}
          </AnimatedList>
        )}
      </Section>
      {usage.data && usage.data.unreadable > 0 && (
        <p className="mt-2 px-1 text-sm text-fg-subtle">
          {usage.data.unreadable}{" "}
          {usage.data.unreadable === 1 ? "volume" : "volumes"} could not be
          measured this time.
        </p>
      )}
    </>
  );
}

const STATE_META: Record<
  DiskState,
  { label: string; tone: Tone | "idle"; pulse?: boolean }
> = {
  active: { label: "In use", tone: "ok" },
  pending: { label: "Setting up…", tone: "warn", pulse: true },
  missing: { label: "Missing — not connected", tone: "bad", pulse: true },
  draining: {
    label: "Being removed — moving data off",
    tone: "warn",
    pulse: true,
  },
  failing: { label: "Could not be added", tone: "bad" },
  blocked: { label: "Needs a decision", tone: "warn" },
  removing: {
    label: "Finishing up — do not unplug yet",
    tone: "warn",
    pulse: true,
  },
  removable: { label: "Safe to unplug", tone: "ok" },
  stale: { label: "Checking…", tone: "idle", pulse: true },
  excluded: { label: "Connected, not in use", tone: "idle" },
  historical: { label: "Not connected", tone: "idle" },
  foreign: { label: "Has data from another system", tone: "warn" },
  unidentified: {
    label: "Can’t identify this disk yet",
    tone: "idle",
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
  osd?: Osd;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const state = diskState(disk);
  const meta = STATE_META[state];
  const isOn = disk.desired === "ON";
  const label = disk.model || disk.device || disk.id;

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

  const Icon = state === "missing" ? WifiOff : disk.is_loop ? Cpu : HardDrive;

  return (
    <div className={cn("px-5 py-4", state === "missing" && "bg-danger-soft")}>
      <div className="flex items-center gap-3">
        <Icon
          className={cn(
            "h-5 w-5 shrink-0",
            state === "missing" ? "text-danger" : "text-fg-muted",
          )}
          strokeWidth={1.5}
        />
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-fg">
            {label}
            {disk.connected && disk.size_bytes > 0 && (
              <span className="ml-2 font-mono font-normal text-fg-muted">
                {formatBytes(disk.size_bytes)}
              </span>
            )}
          </p>
          <p className="mt-0.5 flex items-center gap-1.5 text-sm">
            <span
              aria-hidden
              className={cn(
                "inline-block h-1.5 w-1.5 shrink-0 rounded-full",
                meta.tone === "idle" ? "bg-fg-subtle" : TONE_DOT[meta.tone],
                meta.pulse && "animate-pulse",
              )}
            />
            <span
              className={
                meta.tone === "idle" || meta.tone === "ok"
                  ? "text-fg-muted"
                  : TONE_TEXT[meta.tone]
              }
            >
              <Swap id={meta.label}>{meta.label}</Swap>
              {state === "active" && osd && osd.size_bytes > 0 && (
                <span className={TONE_TEXT[fillTone(osd.utilization)]}>
                  {" · "}
                  <Mono>{Math.round(osd.utilization)}%</Mono> full
                </span>
              )}
              {state === "draining" && <DrainProgress />}
              {disk.is_loop && (
                <span className="text-fg-subtle">
                  {" "}
                  · built into this machine
                </span>
              )}
            </span>
          </p>
        </div>
        {state !== "unidentified" && !confirm && (
          <Switch
            checked={isOn}
            onChange={() => void toggle()}
            disabled={busy}
            label={isOn ? `Stop using ${label}` : `Use ${label}`}
          />
        )}
      </div>
      <Collapse open={Boolean(disk.message)}>
        <p className="pl-8 pt-1 text-sm text-fg-muted">{disk.message}</p>
      </Collapse>
      <Collapse open={Boolean(err)}>
        <p className="pl-8 pt-1 text-sm text-danger">{err}</p>
      </Collapse>
      <Collapse open={confirm}>
        <div className="flex flex-wrap items-center gap-2 pl-8 pt-3">
          <span className="mr-auto text-sm text-fg-muted">
            {isOn
              ? "Its data moves to your other disks first. This can take a while."
              : "Everything on it will be erased."}
          </span>
          <Button size="sm" variant="ghost" onClick={() => setConfirm(false)}>
            Cancel
          </Button>
          <Button
            size="sm"
            variant="danger"
            loading={busy}
            onClick={() => void toggle()}
          >
            {isOn ? "Stop using it" : "Erase and use"}
          </Button>
        </div>
      </Collapse>
    </div>
  );
}

function DisksSection({
  disks,
  osds,
  loading,
  onChanged,
}: {
  disks: Record<string, DiskInfo[]> | undefined;
  osds: Osd[];
  loading: boolean;
  onChanged: () => void;
}) {
  const nodes = disks ? Object.keys(disks).sort() : [];
  const present: [string, DiskInfo][] = [];
  const past: [string, DiskInfo][] = [];
  for (const node of nodes) {
    for (const disk of disks?.[node] ?? []) {
      (diskState(disk) === "historical" ? past : present).push([node, disk]);
    }
  }
  const multiNode = nodes.length > 1;
  const firsts = new Set(
    nodes.flatMap((node) => {
      const first = present.find(([n]) => n === node);
      return first ? [`${node}/${first[1].id}`] : [];
    }),
  );
  const osdFor = (disk: DiskInfo) =>
    disk.osd_id === null ? undefined : osds.find((o) => o.id === disk.osd_id);

  return (
    <Section title="Disks">
      {loading && !disks ? (
        [0, 1].map((i) => (
          <div key={i} className="flex items-center gap-3 px-5 py-4">
            <Skeleton className="h-5 w-5" />
            <div className="flex-1 space-y-2">
              <Skeleton className="h-4 w-40" />
              <Skeleton className="h-3 w-24" />
            </div>
          </div>
        ))
      ) : present.length === 0 && past.length === 0 ? (
        <Row
          label="No disks found yet"
          detail="Plug one in and it will appear here."
        />
      ) : (
        <>
          <AnimatedList
            items={present}
            keyOf={([node, disk]) => `${node}/${disk.id}`}
          >
            {([node, disk]) => (
              <>
                {multiNode && firsts.has(`${node}/${disk.id}`) && (
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
              </>
            )}
          </AnimatedList>
          {present.length === 0 && (
            <Row label="None of the disks this machine has seen are connected right now." />
          )}
          {past.length > 0 && (
            <DisclosureRow
              label={`${past.length} ${past.length === 1 ? "disk" : "disks"} seen before but not connected`}
            >
              <div className="-mx-5 divide-y divide-border opacity-70">
                <AnimatedList
                  items={past}
                  keyOf={([node, disk]) => `${node}/${disk.id}`}
                >
                  {([node, disk]) => (
                    <DiskRow
                      node={node}
                      disk={disk}
                      osd={osdFor(disk)}
                      onChanged={onChanged}
                    />
                  )}
                </AnimatedList>
              </div>
            </DisclosureRow>
          )}
        </>
      )}
    </Section>
  );
}

function ProtectionSection({
  policy,
  overview,
  onChanged,
}: {
  policy: StoragePolicyData;
  overview: StorageOverview | undefined;
  onChanged: () => void;
}) {
  const saved = policy.policy ?? {
    size: policy.target?.size ?? 1,
    failure_domain: (policy.target?.failure_domain === "host"
      ? "host"
      : "osd") as Domain,
  };
  const [size, setSize] = useState(saved.size);
  const [domain, setDomain] = useState<Domain>(saved.failure_domain);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const osds = overview?.osds ?? [];
  const disks = placesFor(osds, "osd");
  const machines = placesFor(osds, "host");
  const places = domain === "osd" ? disks : machines;
  const changed = size !== saved.size || domain !== saved.failure_domain;
  const estimate =
    changed && overview?.space
      ? estimateChange(overview.space, size, places)
      : null;
  const line = protectionLine(policy.target);
  const unit = domain === "osd" ? "disk" : "machine";
  const maxCopies = Math.max(3, saved.size, size);

  function reset() {
    setSize(saved.size);
    setDomain(saved.failure_domain);
    setErr(null);
  }

  async function apply() {
    setBusy(true);
    setErr(null);
    try {
      const d = await api.put<{ ok?: boolean; error?: string }>(
        "/api/storage/policy",
        { size, failure_domain: domain },
      );
      if (d.ok) onChanged();
      else setErr(d.error ?? "Unknown error");
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Section title="Protection">
      <Row
        label="Copies of everything"
        detail={
          size === 1
            ? "No protection against a failed disk"
            : `Survives ${size - 1} ${unit}${size - 1 === 1 ? "" : "s"} failing`
        }
        trailing={
          <Select
            value={size}
            onChange={(e) => setSize(Number(e.target.value))}
            aria-label="Copies of everything"
            className="w-24"
          >
            {Array.from({ length: maxCopies }, (_, i) => i + 1).map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </Select>
        }
      />
      <Row
        label="Keep copies on"
        detail={`${disks} ${disks === 1 ? "disk" : "disks"} on ${machines} ${machines === 1 ? "machine" : "machines"} available`}
        trailing={
          <Select
            value={domain}
            onChange={(e) => setDomain(e.target.value as Domain)}
            aria-label="Keep copies on"
            className="w-48"
          >
            <option value="osd">Different disks</option>
            <option value="host">Different machines</option>
          </Select>
        }
      />
      {changed ? (
        <div className="space-y-3 px-5 py-4">
          <p className="text-sm text-fg">
            Keep {size} {size === 1 ? "copy" : "copies"} of everything on
            different {unit}s?
          </p>
          {size > places && (
            <p className="text-sm text-fg-muted">
              You have {places} {unit}
              {places === 1 ? "" : "s"}, and each copy needs its own. YoLab
              keeps {places} for now and makes the rest when you add{" "}
              {unit === "disk" ? "a disk" : "a machine"}.
            </p>
          )}
          {estimate && (
            <p
              className={cn(
                "text-sm",
                estimate.fit === "impossible"
                  ? "text-danger"
                  : estimate.fit === "tight"
                    ? "text-warning"
                    : "text-fg-muted",
              )}
            >
              {estimate.fit === "impossible" ? (
                <>
                  Not enough room: the extra copies need about{" "}
                  <Bytes value={estimate.extraNeeded} />. Add a disk first.
                </>
              ) : (
                <>
                  About <Bytes value={estimate.freeAfter} /> would be free
                  afterwards
                  {estimate.extraNeeded > 0 && (
                    <>
                      {" "}
                      — the extra copies write about{" "}
                      <Bytes value={estimate.extraNeeded} /> in the background
                    </>
                  )}
                  .
                  {estimate.fit === "tight" &&
                    " It fits, but you will be close to full while the copies are made."}
                </>
              )}
            </p>
          )}
          {err && <p className="text-sm text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button size="sm" variant="ghost" onClick={reset} disabled={busy}>
              Cancel
            </Button>
            <Button
              size="sm"
              variant="secondary"
              loading={busy}
              disabled={estimate?.fit === "impossible"}
              onClick={() => void apply()}
            >
              Apply change
            </Button>
          </div>
        </div>
      ) : (
        line && (
          <Row
            label={<span className={TONE_TEXT[line.tone]}>{line.text}</span>}
          />
        )
      )}
    </Section>
  );
}

function Table({
  head,
  rows,
}: {
  head: string[];
  rows: { key: string; cells: ReactNode[] }[];
}) {
  return (
    <div className="overflow-x-auto rounded-control border border-border">
      <table className="w-full text-xs">
        <thead>
          <tr className="border-b border-border text-left text-fg-muted">
            {head.map((h) => (
              <th key={h} className="whitespace-nowrap px-3 py-2 font-medium">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="divide-y divide-border font-mono tabular-nums text-fg">
          <AnimatedList as="tr" items={rows} keyOf={(r) => r.key}>
            {(r) =>
              r.cells.map((c, i) => (
                <td key={i} className="whitespace-nowrap px-3 py-2">
                  {c}
                </td>
              ))
            }
          </AnimatedList>
        </tbody>
      </table>
    </div>
  );
}

function SwapText({ text }: { text: string }) {
  return <Swap id={text}>{text}</Swap>;
}

function verdict(list: number[] | undefined, id: number): string {
  if (!list) return "…";
  return list.includes(id) ? "yes" : "no";
}

function TechnicalBody({
  overview,
}: {
  overview: StorageOverview | undefined;
}) {
  const status = useApi<{ text: string }>(
    "ceph-status-text",
    "/api/storage/ceph-status",
  );
  const checks = useApi<OsdChecks>("storage-checks", "/api/storage/checks");
  const creds = useResource<{ username: string; password: string }>(
    "ceph-dashboard-creds",
    () =>
      api.get<{ username: string; password: string }>("/api/ceph/dashboard"),
  );
  const [reveal, setReveal] = useState(false);

  return (
    <div className="space-y-5 pt-1">
      <div>
        <div className="mb-2 flex items-center justify-between">
          <p className="text-sm font-medium text-fg">ceph status</p>
          <div className="flex items-center">
            <IconButton
              label="Refresh"
              onClick={() => {
                void status.refresh();
                void checks.refresh();
              }}
            >
              <RefreshCw
                className={cn("h-4 w-4", status.loading && "animate-spin")}
              />
            </IconButton>
            {status.data && (
              <CopyButton value={status.data.text} label="ceph status" />
            )}
          </div>
        </div>
        <pre className="max-h-80 overflow-auto rounded-control bg-surface-2 p-3 font-mono text-xs leading-relaxed text-fg">
          <Swap id={status.data?.text ?? status.error ?? ""} className="block">
            {status.data?.text ??
              (status.error ? status.error : "Asking Ceph…")}
          </Swap>
        </pre>
      </div>

      {overview && overview.osds.length > 0 && (
        <div>
          <p className="mb-2 text-sm font-medium text-fg">Disks (OSDs)</p>
          <Table
            head={[
              "OSD",
              "Host",
              "Class",
              "Size",
              "Used",
              "Free",
              "Fill",
              "PGs",
              "Var",
              "Up",
              "In",
              "Can stop",
              "Can destroy",
            ]}
            rows={overview.osds.map((o) => ({
              key: String(o.id),
              cells: [
                o.name,
                o.host || "—",
                o.class || "—",
                <RollingNumber
                  key="size"
                  value={o.size_bytes}
                  format={formatCephBytes}
                />,
                <RollingNumber
                  key="used"
                  value={o.used_bytes}
                  format={formatCephBytes}
                />,
                <RollingNumber
                  key="free"
                  value={o.avail_bytes}
                  format={formatCephBytes}
                />,
                <RollingNumber
                  key="f"
                  value={o.utilization}
                  format={(n) => `${n.toFixed(1)}%`}
                  className={TONE_TEXT[fillTone(o.utilization)]}
                />,
                <RollingNumber key="pgs" value={o.pgs} />,
                <RollingNumber
                  key="var"
                  value={o.var}
                  format={(n) => n.toFixed(2)}
                />,
                <SwapText key="up" text={o.up ? "up" : "down"} />,
                <SwapText
                  key="in"
                  text={o.weight > 0 && o.reweight > 0 ? "in" : "out"}
                />,
                <SwapText
                  key="stop"
                  text={verdict(checks.data?.ok_to_stop, o.id)}
                />,
                <SwapText
                  key="destroy"
                  text={verdict(checks.data?.safe_to_destroy, o.id)}
                />,
              ],
            }))}
          />
          {checks.error && (
            <p className="mt-1 text-xs text-fg-subtle">
              Safety checks unavailable: {checks.error}
            </p>
          )}
        </div>
      )}

      {overview && overview.pools.length > 0 && (
        <div>
          <p className="mb-2 text-sm font-medium text-fg">Pools</p>
          <Table
            head={["Pool", "Copies", "Min", "Stored", "On disk", "Max avail"]}
            rows={overview.pools.map((p) => ({
              key: String(p.id),
              cells: [
                p.name,
                <RollingNumber key="copies" value={p.copies} />,
                <RollingNumber key="min" value={p.min_copies} />,
                <RollingNumber
                  key="stored"
                  value={p.stored_bytes}
                  format={formatCephBytes}
                />,
                <RollingNumber
                  key="used"
                  value={p.used_bytes}
                  format={formatCephBytes}
                />,
                <RollingNumber
                  key="avail"
                  value={p.max_avail_bytes}
                  format={formatCephBytes}
                />,
              ],
            }))}
          />
        </div>
      )}

      {creds.data && (
        <Card className="divide-y divide-border overflow-hidden p-0">
          <ValueRow
            label="Ceph dashboard user"
            value={creds.data.username}
            copy
          />
          <ValueRow
            label="Ceph dashboard password"
            value={reveal ? creds.data.password : "••••••••••••"}
            trailing={
              <>
                <IconButton
                  label={reveal ? "Hide password" : "Show password"}
                  onClick={() => setReveal((r) => !r)}
                >
                  {reveal ? (
                    <EyeOff className="h-4 w-4" />
                  ) : (
                    <Eye className="h-4 w-4" />
                  )}
                </IconButton>
                <CopyButton
                  value={creds.data.password}
                  label="Ceph dashboard password"
                />
              </>
            }
          />
          <a
            href="/ceph-dashboard/"
            target="_blank"
            rel="noopener noreferrer"
            className="flex items-center gap-3 px-5 py-4 text-sm font-medium text-fg transition-colors hover:bg-surface-2"
          >
            <span className="flex-1">Open Ceph dashboard</span>
            <ExternalLink className="h-4 w-4 text-fg-subtle" />
          </a>
        </Card>
      )}
    </div>
  );
}

export function StoragePage() {
  const movement = useMovement();
  const moving = isVisible(movement.data);
  const pollMs = moving ? POLL_MOVING_MS : POLL_IDLE_MS;

  const overviewRes = useApi<StorageOverview>(
    "storage-overview",
    "/api/storage",
    { pollMs },
  );
  const policyRes = useApi<StoragePolicyData>(
    "storage-policy",
    "/api/storage/policy",
  );
  const disksRes = useApi<Record<string, DiskInfo[]>>(
    "storage-disks",
    "/api/disks",
    { pollMs },
  );

  const overview = overviewRes.data;
  const policy = policyRes.data;
  const blocked = needsAttentionEverywhere(movement.data);

  function refreshAll() {
    void overviewRes.refresh();
    void policyRes.refresh();
    void disksRes.refresh();
    void movement.refresh();
  }

  const banner = pickBanner({
    error: overviewRes.error,
    movementBlocked: blocked,
    overview,
    copies: overview?.space?.copies ?? policy?.target?.size ?? 1,
  });

  return (
    <div>
      <StatusLine
        overview={overview}
        policy={policy}
        movement={movement.data}
      />

      <div className="mt-6 space-y-4 empty:hidden">
        <ForceHealCard />
        {banner === "movement" ? (
          <DataMovementCard />
        ) : (
          banner && (
            <Banner tone={banner.tone} title={banner.title}>
              {banner.body}
            </Banner>
          )
        )}
      </div>

      <SpaceSection overview={overview} loading={overviewRes.loading} />
      {!blocked && <DataMovementCard className="mt-4" />}

      <UsageSection imagesBytes={overview?.space?.images_bytes ?? 0} />

      <DisksSection
        disks={disksRes.data}
        osds={overview?.osds ?? []}
        loading={disksRes.loading}
        onChanged={refreshAll}
      />

      {policy && (
        <ProtectionSection
          key={`${policy.policy?.size}-${policy.policy?.failure_domain}`}
          policy={policy}
          overview={overview}
          onChanged={refreshAll}
        />
      )}

      <section className="mt-8">
        <Card className="overflow-hidden p-0">
          <DisclosureRow
            label="Technical details"
            detail="Raw Ceph status, every OSD and pool, and the Ceph dashboard"
          >
            <TechnicalBody overview={overview} />
          </DisclosureRow>
        </Card>
      </section>
    </div>
  );
}
