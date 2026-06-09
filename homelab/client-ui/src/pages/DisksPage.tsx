import { useEffect, useRef, useState } from "react"
import { HardDrive, Server, CheckCircle, Clock, AlertCircle, Lock, ChevronUp, ChevronDown } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import type { CephStatus, DiskInfo, EjectStatus, PriorityItem, PriorityUpdateRequest, SystemOsdInfo } from "@/types/disk"

// ── helpers ───────────────────────────────────────────────────────────────────

function fmt(bytes: number | null | undefined): string {
  if (!bytes) return "—"
  const units = ["B", "KB", "MB", "GB", "TB"]
  let v = bytes, i = 0
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++ }
  return `${v.toFixed(1)} ${units[i]}`
}

function parseBytes(s: string): number {
  const units: Record<string, number> = { K: 1024, M: 1024 ** 2, G: 1024 ** 3, T: 1024 ** 4, P: 1024 ** 5 }
  const m = s.trim().toUpperCase().replace(/I$/, "").match(/^(\d+(?:\.\d+)?)([KMGTP]?)$/)
  if (!m) return NaN
  return parseFloat(m[1]) * (units[m[2]] ?? 1)
}

function UsageBar({ used, total, className = "" }: { used: number; total: number; className?: string }) {
  const pct = total > 0 ? Math.min(100, (used / total) * 100) : 0
  const color = pct > 85 ? "#f87171" : pct > 65 ? "#fbbf24" : "#a78bfa"
  return (
    <div className={`h-1.5 rounded-full bg-[#27272a] overflow-hidden ${className}`}>
      <div className="h-full rounded-full transition-all" style={{ width: `${pct.toFixed(1)}%`, background: color }} />
    </div>
  )
}

// ── confirmation modal ────────────────────────────────────────────────────────

interface ModalProps {
  title: string
  children: React.ReactNode
  confirmLabel: string
  confirmDestructive?: boolean
  onConfirm: () => void
  onCancel: () => void
}

function Modal({ title, children, confirmLabel, confirmDestructive, onConfirm, onCancel }: ModalProps) {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="bg-[#18181b] border border-[#3f3f46] rounded-xl p-6 max-w-md w-full mx-4 shadow-2xl">
        <h3 className="font-semibold text-[#fafafa] mb-3">{title}</h3>
        <div className="text-sm text-[#a1a1aa] mb-6">{children}</div>
        <div className="flex justify-end gap-2">
          <Button variant="outline" size="sm" onClick={onCancel}>Cancel</Button>
          <Button
            size="sm"
            onClick={onConfirm}
            className={confirmDestructive ? "bg-[#7f1d1d] hover:bg-[#991b1b] border-[#7f1d1d] text-white" : ""}
          >
            {confirmLabel}
          </Button>
        </div>
      </div>
    </div>
  )
}

// ── storage overview ──────────────────────────────────────────────────────────

function StorageOverview({ status }: { status: CephStatus | null }) {
  if (!status?.available || !status.total_bytes) return null
  const { used_bytes: used, total_bytes: total } = status
  const pct = Math.round((used / total) * 100)
  const color = pct > 85 ? "#f87171" : pct > 65 ? "#fbbf24" : "#a78bfa"
  return (
    <Card>
      <CardContent className="pt-4 pb-4">
        <div className="space-y-2">
          <div className="flex justify-between text-xs">
            <span className="text-[#fafafa] font-medium">{fmt(used)} used</span>
            <span className="text-[#71717a]">{fmt(total - used)} free · {pct}%</span>
          </div>
          <div className="h-2 rounded-full bg-[#27272a] overflow-hidden">
            <div className="h-full rounded-full transition-all" style={{ width: `${pct}%`, background: color }} />
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

// ── system disk card ──────────────────────────────────────────────────────────

function SystemDiskCard({ disk, osd, onResize }: { disk: DiskInfo; osd: SystemOsdInfo | null; onResize: () => void }) {
  const [sizeInput, setSizeInput] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  const targetBytes = parseBytes(sizeInput)
  const isShrink = osd?.size_bytes != null && !isNaN(targetBytes) && targetBytes < osd.size_bytes

  async function resize() {
    if (!sizeInput.trim()) return
    if (isShrink && !confirm(
      "Shrinking removes the built-in OSD and recreates it at the new size.\n" +
      "Ceph will rebalance data to other disks first — this may take time.\n\nContinue?"
    )) return
    setBusy(true)
    setError("")
    const r = await fetch("/api/disks/system-osd", {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ size: sizeInput.trim() }),
    })
    setBusy(false)
    if (r.ok) { setSizeInput(""); onResize() }
    else {
      const d = (await r.json()) as { detail?: string }
      setError(d.detail ?? "Failed")
    }
  }

  const osdStatus = !osd ? null : osd.ceph_osd_id !== null ? "active" : "starting"

  return (
    <Card className="border-[#27272a]">
      <CardContent className="pt-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md bg-[#27272a] p-1.5 flex-shrink-0">
            <Server className="h-4 w-4 text-[#71717a]" strokeWidth={1.75} />
          </div>
          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <div className="flex items-baseline gap-2">
                <span className="font-medium text-[#fafafa] text-sm">{disk.model || disk.name}</span>
                {disk.model && <span className="text-xs text-[#52525b] font-mono">{disk.name}</span>}
              </div>
              <div className="flex items-center gap-2">
                <span className="text-xs text-[#52525b]">System · {fmt(disk.size_bytes)}</span>
              </div>
            </div>
            <div className="text-xs text-[#52525b] mt-0.5">{disk.hostname}</div>
            {osd && (
              <div className="mt-3 pt-3 border-t border-[#27272a]">
                <div className="flex items-center justify-between gap-2 flex-wrap">
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-[#71717a]">Built-in storage</span>
                    <span className="text-xs text-[#3f3f46]">·</span>
                    <span className="text-xs text-[#52525b]">{fmt(osd.size_bytes ?? 0)} allocated</span>
                    {osdStatus === "active" && <span className="text-xs text-[#4ade80]">Active</span>}
                    {osdStatus === "starting" && (
                      <div className="relative group inline-flex">
                        <span className="text-xs text-[#fbbf24] cursor-default">Starting…</span>
                        <div className="absolute bottom-full mb-2 left-0 w-64 bg-[#27272a] border border-[#3f3f46] rounded-lg px-3 py-2 text-xs text-[#a1a1aa] hidden group-hover:block z-10">
                          Storage is initializing. This is normal on first boot — usually takes 1–2 minutes.
                        </div>
                      </div>
                    )}
                  </div>
                  <div className="flex items-center gap-2">
                    <input
                      className="w-24 bg-[#18181b] border border-[#3f3f46] rounded px-2 py-1 text-xs text-[#fafafa] outline-none focus:border-[#a78bfa]"
                      placeholder="e.g. 400G"
                      value={sizeInput}
                      onChange={(e) => setSizeInput(e.target.value)}
                    />
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => void resize()}
                      disabled={busy || !sizeInput.trim()}
                      className={isShrink ? "text-[#fbbf24] border-[#78350f] hover:border-[#fbbf24]" : ""}
                    >
                      {busy ? "Resizing…" : isShrink ? "Shrink" : "Extend"}
                    </Button>
                  </div>
                </div>
                {isShrink && (
                  <p className="text-xs text-[#fbbf24] mt-1.5">
                    Shrinking moves data to other disks first — requires at least one other active disk.
                  </p>
                )}
                {error && <p className="text-xs text-[#f87171] mt-1.5">{error}</p>}
              </div>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

// ── priority row (active or waiting disk in the ordered list) ─────────────────

interface PriorityRowProps {
  item: PriorityItem
  isFirst: boolean
  isLast: boolean
  isOnlyWaiting: boolean
  onMove: (direction: "up" | "down") => void
  onEjected: () => void
  onRemoved: () => void
}

function PriorityRow({ item, isFirst, isLast, onMove, onEjected, onRemoved }: PriorityRowProps) {
  const [confirming, setConfirming] = useState(false)
  const [ejecting, setEjecting] = useState(item.state === "ejecting")
  const [ejectDone, setEjectDone] = useState(false)
  const [ejectError, setEjectError] = useState("")
  const [removeConfirming, setRemoveConfirming] = useState(false)
  const [removeBusy, setRemoveBusy] = useState(false)
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  useEffect(() => {
    if (item.state === "ejecting" && !ejecting) {
      setEjecting(true)
      startEjectPolling()
    }
  }, [item.state])

  useEffect(() => () => { if (pollRef.current) clearInterval(pollRef.current) }, [])

  function startEjectPolling() {
    pollRef.current = setInterval(async () => {
      try {
        const r = await fetch(`/api/disks/eject/${item.disk_name}/status`)
        if (!r.ok) return
        const s = (await r.json()) as EjectStatus
        if (s.safe_to_unplug) {
          clearInterval(pollRef.current!)
          setEjecting(false)
          setEjectDone(true)
          setTimeout(onEjected, 3000)
        }
      } catch { /* keep polling */ }
    }, 3000)
  }

  async function confirmEject() {
    setConfirming(false)
    setEjectError("")
    const r = await fetch("/api/disks/eject", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disk_name: item.disk_name, host: item.host }),
    })
    if (!r.ok) {
      const d = (await r.json()) as { detail?: { reason?: string; used_bytes?: number; other_free_bytes?: number } | string }
      const detail = d.detail
      if (typeof detail === "object" && detail?.reason === "not_enough_space") {
        setEjectError(`Not enough space — need ${fmt(detail.used_bytes)} free, only ${fmt(detail.other_free_bytes)} available`)
      } else {
        setEjectError(typeof detail === "string" ? detail : "Failed to start ejection")
      }
      return
    }
    setEjecting(true)
    startEjectPolling()
  }

  async function confirmRemove() {
    setRemoveConfirming(false)
    setRemoveBusy(true)
    await fetch(`/api/disks/queue/${item.disk_name}?host=${encodeURIComponent(item.host)}`, { method: "DELETE" })
    setRemoveBusy(false)
    onRemoved()
  }

  const isActive = item.state === "active" || item.state === "ejecting"
  const isWaiting = item.state === "waiting"
  const usedPct = item.used_bytes && item.size_bytes ? Math.round((item.used_bytes / item.size_bytes) * 100) : null

  const iconBg = isActive ? "#1a2e1a" : "#2d2a1a"
  const iconColor = isActive ? "#4ade80" : "#fbbf24"
  const stateLabel = isActive ? (
    item.state === "ejecting" ? <span className="text-xs text-[#fbbf24]">Ejecting…</span>
    : <span className="text-xs text-[#4ade80]">Active</span>
  ) : (
    <span className="text-xs text-[#fbbf24]">Waiting #{item.position}</span>
  )

  return (
    <>
      <Card>
        <CardContent className="pt-5">
          <div className="flex items-start gap-3">

            {/* position controls */}
            <div className="flex flex-col items-center gap-0.5 mt-0.5 flex-shrink-0">
              {isActive ? (
                <div className="p-1">
                  <Lock className="h-3.5 w-3.5 text-[#3f3f46]" strokeWidth={1.75} />
                </div>
              ) : (
                <>
                  <button
                    onClick={() => onMove("up")}
                    disabled={isFirst}
                    className="p-0.5 rounded hover:bg-[#27272a] disabled:opacity-20 disabled:cursor-not-allowed transition-colors"
                    title="Move up"
                  >
                    <ChevronUp className="h-3.5 w-3.5 text-[#71717a]" strokeWidth={2} />
                  </button>
                  <button
                    onClick={() => onMove("down")}
                    disabled={isLast}
                    className="p-0.5 rounded hover:bg-[#27272a] disabled:opacity-20 disabled:cursor-not-allowed transition-colors"
                    title="Move down"
                  >
                    <ChevronDown className="h-3.5 w-3.5 text-[#71717a]" strokeWidth={2} />
                  </button>
                </>
              )}
            </div>

            {/* disk icon */}
            <div className="mt-0.5 rounded-md p-1.5 flex-shrink-0" style={{ background: iconBg }}>
              <HardDrive className="h-4 w-4" style={{ color: iconColor }} strokeWidth={1.75} />
            </div>

            {/* content */}
            <div className="flex-1 min-w-0">
              <div className="flex items-center justify-between gap-2 flex-wrap">
                <div className="flex items-baseline gap-2">
                  <span className="font-medium text-[#fafafa] text-sm">{item.model || item.disk_name}</span>
                  {item.model && <span className="text-xs text-[#52525b] font-mono">{item.disk_name}</span>}
                </div>
                <div className="flex items-center gap-2">
                  {stateLabel}
                  <span className="text-xs text-[#52525b]">·</span>
                  <span className="text-xs text-[#71717a]">{fmt(item.size_bytes)}</span>
                </div>
              </div>

              <div className="text-xs text-[#52525b] mt-0.5">{item.hostname}</div>

              {isActive && item.used_bytes != null && item.size_bytes > 0 && (
                <div className="mt-2 space-y-1">
                  <UsageBar used={item.used_bytes} total={item.size_bytes} />
                  <span className="text-xs text-[#71717a]">{fmt(item.used_bytes)} used · {usedPct}%</span>
                </div>
              )}

              {/* active disk actions */}
              {isActive && (
                <div className="flex items-center gap-2 mt-3 flex-wrap">
                  {ejectDone ? (
                    <div className="flex items-center gap-2 text-sm text-[#4ade80]">
                      <CheckCircle className="h-4 w-4" />
                      Safe to unplug
                    </div>
                  ) : ejecting ? (
                    <div className="flex items-center gap-2 text-sm text-[#fbbf24]">
                      <Clock className="h-4 w-4 animate-spin" />
                      Clearing disk data…
                    </div>
                  ) : (
                    <>
                      {ejectError && <span className="text-xs text-[#f87171]">{ejectError}</span>}
                      <div className="relative group">
                        <Button
                          variant="outline"
                          size="sm"
                          onClick={() => item.can_eject && setConfirming(true)}
                          disabled={!item.can_eject}
                          className={!item.can_eject ? "opacity-40 cursor-not-allowed" : ""}
                        >
                          Eject
                        </Button>
                        {!item.can_eject && (
                          <div className="absolute bottom-full mb-2 left-1/2 -translate-x-1/2 w-56 bg-[#27272a] border border-[#3f3f46] rounded-lg px-3 py-2 text-xs text-[#a1a1aa] hidden group-hover:block z-10 text-center">
                            Not enough free space on other disks to absorb this disk's data
                          </div>
                        )}
                      </div>
                    </>
                  )}
                </div>
              )}

              {/* waiting disk actions */}
              {isWaiting && (
                <div className="flex items-center justify-between mt-3 flex-wrap gap-2">
                  <span className="text-xs text-[#52525b]">Will activate when storage reaches 80%</span>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setRemoveConfirming(true)}
                    disabled={removeBusy}
                  >
                    {removeBusy ? "Removing…" : "Remove"}
                  </Button>
                </div>
              )}
            </div>
          </div>
        </CardContent>
      </Card>

      {confirming && (
        <Modal
          title={`Eject ${item.model || item.disk_name}?`}
          confirmLabel="Start ejection"
          onConfirm={() => void confirmEject()}
          onCancel={() => setConfirming(false)}
        >
          <p>
            {item.used_bytes
              ? <>Your apps will keep running. <strong className="text-[#fafafa]">{fmt(item.used_bytes)}</strong> of data will be moved to other disks first. This may take a few minutes.</>
              : <>The disk will be safely removed. Your apps will keep running.</>}
          </p>
        </Modal>
      )}

      {removeConfirming && (
        <Modal
          title={`Remove ${item.model || item.disk_name} from queue?`}
          confirmLabel="Remove"
          confirmDestructive
          onConfirm={() => void confirmRemove()}
          onCancel={() => setRemoveConfirming(false)}
        >
          <p>This disk will be removed from the priority list. Plug it back in and it will be detected again.</p>
        </Modal>
      )}
    </>
  )
}

// ── unformatted disk card ─────────────────────────────────────────────────────

function UnformattedDiskCard({ disk, onAdded }: { disk: DiskInfo; onAdded: () => void }) {
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  async function confirmAdd() {
    setConfirming(false)
    setBusy(true)
    setError("")
    const r = await fetch("/api/disks/add", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
    })
    setBusy(false)
    if (r.ok) {
      setTimeout(onAdded, 3000)
    } else {
      const d = (await r.json()) as { detail?: string }
      setError(d.detail ?? "Failed")
    }
  }

  return (
    <>
      <Card>
        <CardContent className="pt-5">
          <div className="flex items-start gap-3">
            <div className="mt-0.5 rounded-md bg-[#27272a] p-1.5 flex-shrink-0">
              <HardDrive className="h-4 w-4 text-[#52525b]" strokeWidth={1.75} />
            </div>
            <div className="flex-1 min-w-0">
              <div className="flex items-center justify-between gap-2 flex-wrap">
                <div className="flex items-baseline gap-2">
                  <span className="font-medium text-[#fafafa] text-sm">{disk.model || disk.name}</span>
                  {disk.model && <span className="text-xs text-[#52525b] font-mono">{disk.name}</span>}
                </div>
                <div className="flex items-center gap-2">
                  <span className="text-xs text-[#52525b]">Available · {fmt(disk.size_bytes)}</span>
                </div>
              </div>
              <div className="text-xs text-[#52525b] mt-0.5">{disk.hostname}</div>
              <div className="flex items-center justify-between mt-3 flex-wrap gap-2">
                {error && (
                  <span className="flex items-center gap-1 text-xs text-[#f87171]">
                    <AlertCircle className="h-3 w-3" /> {error}
                  </span>
                )}
                <div className="ml-auto">
                  <Button variant="outline" size="sm" onClick={() => setConfirming(true)} disabled={busy}>
                    {busy ? "Adding…" : "Add to storage"}
                  </Button>
                </div>
              </div>
            </div>
          </div>
        </CardContent>
      </Card>

      {confirming && (
        <Modal
          title={`Add ${disk.model || disk.name} to storage?`}
          confirmLabel="Add to storage"
          confirmDestructive={disk.fs_type !== null}
          onConfirm={() => void confirmAdd()}
          onCancel={() => setConfirming(false)}
        >
          {disk.fs_type !== null
            ? <p>All existing data on this disk will be <strong className="text-[#fafafa]">permanently erased</strong>. It will join the storage pool automatically when needed.</p>
            : <p>This disk is blank and will be added to the storage pool. It will activate automatically when your storage gets full.</p>
          }
        </Modal>
      )}
    </>
  )
}

// ── page ──────────────────────────────────────────────────────────────────────

export function DisksPage() {
  const [flatDisks, setFlatDisks] = useState<DiskInfo[]>([])
  const [priority, setPriority] = useState<PriorityItem[]>([])
  const [ceph, setCeph] = useState<CephStatus | null>(null)
  const [osd, setOsd] = useState<SystemOsdInfo | null>(null)
  const [savingOrder, setSavingOrder] = useState(false)

  function load() {
    fetch("/api/disks")
      .then((r) => r.json())
      .then((d) => setFlatDisks(d as DiskInfo[]))
      .catch(() => { })
    fetch("/api/disks/priority")
      .then((r) => r.json())
      .then((d) => setPriority(d as PriorityItem[]))
      .catch(() => { })
    fetch("/api/ceph/status")
      .then((r) => r.json())
      .then((d) => setCeph(d as CephStatus))
      .catch(() => { })
    fetch("/api/disks/system-osd")
      .then((r) => r.json())
      .then((d) => setOsd(d as SystemOsdInfo))
      .catch(() => { })
  }

  useEffect(() => {
    load()
    const interval = setInterval(load, 10_000)
    return () => clearInterval(interval)
  }, [])

  async function moveItem(index: number, direction: "up" | "down") {
    const next = [...priority]
    const target = direction === "up" ? index - 1 : index + 1
    if (target < 0 || target >= next.length) return
    // Can't move above an active disk or below an active disk boundary
    if (next[target].state === "active" || next[target].state === "ejecting") return
    ;[next[index], next[target]] = [next[target], next[index]]
    // Optimistic update
    setPriority(next.map((item, i) => ({ ...item, position: i + 1 })))
    setSavingOrder(true)
    try {
      const body: PriorityUpdateRequest = { entries: next.map((item) => ({ host: item.host, disk_name: item.disk_name })) }
      await fetch("/api/disks/priority", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      })
    } finally {
      setSavingOrder(false)
      load()
    }
  }

  const systemDisks = flatDisks.filter((d) => d.state === "system")
  const unformatted = flatDisks.filter((d) => d.state === "unformatted")

  // Waiting items in the priority list (for up/down boundary detection)
  const waitingIndices = priority
    .map((item, i) => ({ item, i }))
    .filter(({ item }) => item.state === "waiting")

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
        <p className="text-sm text-[#71717a] mt-0.5">Total capacity across all active disks.</p>
      </div>

      <StorageOverview status={ceph} />

      {/* System disk */}
      {systemDisks.map((d) => (
        <SystemDiskCard key={`${d.host}:${d.name}`} disk={d} osd={osd} onResize={load} />
      ))}

      {/* Priority list — active + waiting in order */}
      {priority.length > 0 && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">
              Storage disks
            </h2>
            {savingOrder && <span className="text-xs text-[#52525b]">Saving…</span>}
          </div>
          <div className="space-y-3">
            {priority.map((item, i) => {
              const isWaiting = item.state === "waiting"
              // For up/down: find position among waiting items only
              const waitingIdx = waitingIndices.findIndex(({ i: wi }) => wi === i)
              const isFirstWaiting = isWaiting && waitingIdx === 0
              const isLastWaiting = isWaiting && waitingIdx === waitingIndices.length - 1
              return (
                <PriorityRow
                  key={`${item.host}:${item.disk_name}`}
                  item={item}
                  isFirst={isFirstWaiting}
                  isLast={isLastWaiting}
                  isOnlyWaiting={waitingIndices.length === 1}
                  onMove={(dir) => void moveItem(i, dir)}
                  onEjected={load}
                  onRemoved={load}
                />
              )
            })}
          </div>
        </div>
      )}

      {/* Unformatted / available disks */}
      {unformatted.length > 0 && (
        <div>
          <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b] mb-3">Available disks</h2>
          <div className="space-y-3">
            {unformatted.map((d) => (
              <UnformattedDiskCard key={`${d.host}:${d.name}`} disk={d} onAdded={load} />
            ))}
          </div>
        </div>
      )}
    </div>
  )
}
