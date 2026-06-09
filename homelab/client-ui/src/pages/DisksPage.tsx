import { useEffect, useRef, useState } from "react"
import { HardDrive, Server, CheckCircle, Clock, AlertCircle } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import type { CephStatus, DiskInfo, EjectStatus, SystemOsdInfo } from "@/types/disk"

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

// ── capacity bar ──────────────────────────────────────────────────────────────

function CapacityHeader({ status }: { status: CephStatus | null }) {
  if (!status?.available) return null
  const { used_bytes: used, total_bytes: total } = status
  if (!total) return null
  const pct = Math.round((used / total) * 100)
  return (
    <div className="space-y-2">
      <div className="flex justify-between text-xs text-[#71717a]">
        <span>{fmt(used)} used</span>
        <span>{fmt(total - used)} free · {pct}%</span>
      </div>
      <UsageBar used={used} total={total} />
    </div>
  )
}

// ── active disk card ──────────────────────────────────────────────────────────

function ActiveDiskCard({ disk, onEjected }: { disk: DiskInfo; onEjected: () => void }) {
  const [confirming, setConfirming] = useState(false)
  const [ejecting, setEjecting] = useState(disk.state === "ejecting")
  const [done, setDone] = useState(false)
  const [error, setError] = useState("")
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  useEffect(() => {
    if (disk.state === "ejecting" && !ejecting) {
      setEjecting(true)
      startPolling()
    }
  }, [disk.state])

  function startPolling() {
    pollRef.current = setInterval(async () => {
      try {
        const r = await fetch(`/api/disks/eject/${disk.name}/status`)
        if (!r.ok) return
        const s = (await r.json()) as EjectStatus
        if (s.safe_to_unplug) {
          clearInterval(pollRef.current!)
          setEjecting(false)
          setDone(true)
          setTimeout(onEjected, 3000)
        }
      } catch { /* keep polling */ }
    }, 3000)
  }

  async function confirmEject() {
    setConfirming(false)
    setError("")
    const r = await fetch("/api/disks/eject", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
    })
    if (!r.ok) {
      const d = (await r.json()) as { detail?: { reason?: string; used_bytes?: number; other_free_bytes?: number } | string }
      const detail = d.detail
      if (typeof detail === "object" && detail?.reason === "not_enough_space") {
        setError(`Not enough space — need ${fmt(detail.used_bytes)} free, only ${fmt(detail.other_free_bytes)} available`)
      } else {
        setError(typeof detail === "string" ? detail : "Failed to start ejection")
      }
      return
    }
    setEjecting(true)
    startPolling()
  }

  useEffect(() => () => { if (pollRef.current) clearInterval(pollRef.current) }, [])

  const usedPct = disk.used_bytes && disk.size_bytes
    ? Math.round((disk.used_bytes / disk.size_bytes) * 100)
    : null

  return (
    <>
      <Card>
        <CardContent className="pt-5">
          <div className="flex items-start gap-3">
            <div className="mt-0.5 rounded-md bg-[#1a2e1a] p-1.5 flex-shrink-0">
              <HardDrive className="h-4 w-4 text-[#4ade80]" strokeWidth={1.75} />
            </div>
            <div className="flex-1 min-w-0">
              <div className="flex items-center justify-between gap-2 flex-wrap">
                <div className="flex items-baseline gap-2">
                  <span className="font-medium text-[#fafafa] text-sm">{disk.model || disk.name}</span>
                  {disk.model && <span className="text-xs text-[#52525b] font-mono">{disk.name}</span>}
                </div>
                <div className="flex items-center gap-2">
                  <span className="text-xs text-[#4ade80]">Active</span>
                  <span className="text-xs text-[#52525b]">·</span>
                  <span className="text-xs text-[#71717a]">{fmt(disk.size_bytes)}</span>
                </div>
              </div>

              {disk.used_bytes != null && disk.size_bytes > 0 && (
                <div className="mt-2 space-y-1">
                  <UsageBar used={disk.used_bytes} total={disk.size_bytes} />
                  <span className="text-xs text-[#71717a]">
                    {fmt(disk.used_bytes)} used · {usedPct}%
                  </span>
                </div>
              )}

              {done ? (
                <div className="flex items-center gap-2 mt-3 text-sm text-[#4ade80]">
                  <CheckCircle className="h-4 w-4" />
                  Safe to unplug
                </div>
              ) : ejecting ? (
                <div className="flex items-center gap-2 mt-3 text-sm text-[#fbbf24]">
                  <Clock className="h-4 w-4 animate-spin" />
                  Clearing disk data…
                </div>
              ) : (
                <div className="flex items-center gap-2 mt-3 flex-wrap">
                  {error && <span className="text-xs text-[#f87171]">{error}</span>}
                  <div className="relative group">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => disk.can_eject && setConfirming(true)}
                      disabled={!disk.can_eject}
                      className={!disk.can_eject ? "opacity-40 cursor-not-allowed" : ""}
                    >
                      Eject
                    </Button>
                    {!disk.can_eject && (
                      <div className="absolute bottom-full mb-2 left-1/2 -translate-x-1/2 w-56 bg-[#27272a] border border-[#3f3f46] rounded-lg px-3 py-2 text-xs text-[#a1a1aa] hidden group-hover:block z-10 text-center">
                        Not enough free space on other disks to absorb this disk's data
                      </div>
                    )}
                  </div>
                </div>
              )}
            </div>
          </div>
        </CardContent>
      </Card>

      {confirming && (
        <Modal
          title={`Eject ${disk.model || disk.name}?`}
          confirmLabel="Start ejection"
          onConfirm={() => void confirmEject()}
          onCancel={() => setConfirming(false)}
        >
          <p>
            {disk.used_bytes
              ? <>Your apps will keep running. <strong className="text-[#fafafa]">{fmt(disk.used_bytes)}</strong> of data will be moved to other disks first. This may take a few minutes.</>
              : <>The disk will be safely removed. Your apps will keep running.</>}
          </p>
        </Modal>
      )}
    </>
  )
}

// ── waiting disk card ─────────────────────────────────────────────────────────

function WaitingDiskCard({ disk, onRemoved }: { disk: DiskInfo; onRemoved: () => void }) {
  const [busy, setBusy] = useState(false)

  async function remove() {
    setBusy(true)
    await fetch(`/api/disks/queue/${disk.name}?host=${encodeURIComponent(disk.host)}`, { method: "DELETE" })
    onRemoved()
  }

  return (
    <Card>
      <CardContent className="pt-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md bg-[#2d2a1a] p-1.5 flex-shrink-0">
            <HardDrive className="h-4 w-4 text-[#fbbf24]" strokeWidth={1.75} />
          </div>
          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <div className="flex items-baseline gap-2">
                <span className="font-medium text-[#fafafa] text-sm">{disk.model || disk.name}</span>
                {disk.model && <span className="text-xs text-[#52525b] font-mono">{disk.name}</span>}
              </div>
              <div className="flex items-center gap-2">
                <span className="text-xs text-[#fbbf24]">Queued #{disk.queue_position}</span>
                <span className="text-xs text-[#52525b]">·</span>
                <span className="text-xs text-[#71717a]">{fmt(disk.size_bytes)}</span>
              </div>
            </div>
            <div className="flex items-center justify-between mt-3 flex-wrap gap-2">
              <span className="text-xs text-[#52525b]">Will activate when storage reaches 80%</span>
              <Button variant="outline" size="sm" onClick={() => void remove()} disabled={busy}>
                {busy ? "Removing…" : "Remove from queue"}
              </Button>
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
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
                  <span className="text-xs text-[#52525b]">New disk</span>
                  <span className="text-xs text-[#52525b]">·</span>
                  <span className="text-xs text-[#71717a]">{fmt(disk.size_bytes)}</span>
                </div>
              </div>
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
          confirmDestructive
          onConfirm={() => void confirmAdd()}
          onCancel={() => setConfirming(false)}
        >
          <p>
            All existing data on this disk will be <strong className="text-[#fafafa]">permanently erased</strong>.
            The disk will be added to your storage pool and activated when your current storage reaches 80%.
          </p>
        </Modal>
      )}
    </>
  )
}

// ── system OSD card ───────────────────────────────────────────────────────────

function SystemOsdCard() {
  const [osd, setOsd] = useState<SystemOsdInfo | null>(null)
  const [sizeInput, setSizeInput] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")

  function load() {
    fetch("/api/disks/system-osd")
      .then((r) => r.json())
      .then((d) => setOsd(d as SystemOsdInfo))
      .catch(() => { })
  }

  useEffect(() => { load() }, [])

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
    if (r.ok) { setSizeInput(""); load() }
    else {
      const d = (await r.json()) as { detail?: string }
      setError(d.detail ?? "Failed")
    }
  }

  if (!osd) return null

  return (
    <Card className="border-dashed border-[#3f3f46]">
      <CardContent className="pt-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md bg-[#27272a] p-1.5 flex-shrink-0">
            <Server className="h-4 w-4 text-[#71717a]" strokeWidth={1.75} />
          </div>
          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <span className="font-medium text-[#a1a1aa] text-sm">Built-in storage</span>
              <div className="flex items-center gap-2">
                {osd.ceph_osd_id !== null
                  ? <span className="text-xs text-[#4ade80]">Active</span>
                  : <span className="text-xs text-[#fbbf24]">Starting…</span>}
                <span className="text-xs text-[#52525b]">·</span>
                <span className="text-xs text-[#71717a]">{fmt(osd.size_bytes ?? 0)} allocated</span>
              </div>
            </div>
            <div className="flex items-center gap-2 mt-3">
              <input
                className="w-28 bg-[#18181b] border border-[#3f3f46] rounded px-2 py-1 text-xs text-[#fafafa] outline-none focus:border-[#a78bfa]"
                placeholder="e.g. 400G, 1T"
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
            {isShrink && (
              <p className="text-xs text-[#fbbf24] mt-1.5">
                Shrinking moves data to other disks first — requires at least one other active disk.
              </p>
            )}
            {error && <p className="text-xs text-[#f87171] mt-1.5">{error}</p>}
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

// ── page ──────────────────────────────────────────────────────────────────────

export function DisksPage() {
  const [disks, setDisks] = useState<DiskInfo[]>([])
  const [ceph, setCeph] = useState<CephStatus | null>(null)

  function load() {
    fetch("/api/disks")
      .then((r) => r.json())
      .then((d) => setDisks(d as DiskInfo[]))
      .catch(() => { })
    fetch("/api/ceph/status")
      .then((r) => r.json())
      .then((d) => setCeph(d as CephStatus))
      .catch(() => { })
  }

  useEffect(() => { load() }, [])

  const active = disks.filter((d) => d.state === "active" || d.state === "ejecting")
  const waiting = disks.filter((d) => d.state === "waiting")
  const unformatted = disks.filter((d) => d.state === "unformatted")
  const system = disks.filter((d) => d.state === "system")

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
        <p className="text-sm text-[#71717a] mt-0.5">Disks fill up one at a time. Next disk activates automatically at 80%.</p>
      </div>

      <CapacityHeader status={ceph} />

      {active.length > 0 && (
        <div className="space-y-3">
          {active.map((d) => (
            <ActiveDiskCard key={`${d.host}:${d.name}`} disk={d} onEjected={load} />
          ))}
        </div>
      )}

      {waiting.length > 0 && (
        <div className="space-y-3">
          {waiting.map((d) => (
            <WaitingDiskCard key={`${d.host}:${d.name}`} disk={d} onRemoved={load} />
          ))}
        </div>
      )}

      {unformatted.length > 0 && (
        <div className="space-y-3">
          {unformatted.map((d) => (
            <UnformattedDiskCard key={`${d.host}:${d.name}`} disk={d} onAdded={load} />
          ))}
        </div>
      )}

      <SystemOsdCard />

      {system.map((d) => (
        <Card key={`${d.host}:${d.name}`} className="border-dashed border-[#3f3f46]">
          <CardContent className="pt-5">
            <div className="flex items-center gap-3">
              <div className="rounded-md bg-[#27272a] p-1.5 flex-shrink-0">
                <Server className="h-4 w-4 text-[#52525b]" strokeWidth={1.75} />
              </div>
              <div className="flex items-baseline gap-2 flex-1">
                <span className="text-sm text-[#71717a]">{d.model || d.name}</span>
                {d.model && <span className="text-xs text-[#3f3f46] font-mono">{d.name}</span>}
              </div>
              <span className="text-xs text-[#52525b]">System disk · {fmt(d.size_bytes)}</span>
            </div>
          </CardContent>
        </Card>
      ))}
    </div>
  )
}
