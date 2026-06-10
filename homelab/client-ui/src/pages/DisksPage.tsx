import { useEffect, useState } from "react"
import { HardDrive, ChevronUp, ChevronDown } from "lucide-react"
import { Card, CardContent } from "@/components/ui/card"
import type { CephStatus, DiskItem, DiskOrderRequest } from "@/types/disk"

function fmt(bytes: number | null | undefined): string {
  if (!bytes) return "—"
  const units = ["B", "KB", "MB", "GB", "TB"]
  let v = bytes, i = 0
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++ }
  return `${v.toFixed(1)} ${units[i]}`
}

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

function DiskRow({ disk, isFirst, isLast, onMove }: {
  disk: DiskItem
  isFirst: boolean
  isLast: boolean
  onMove: (dir: "up" | "down") => void
}) {
  const usedPct = disk.used_bytes && disk.size_bytes
    ? Math.round((disk.used_bytes / disk.size_bytes) * 100)
    : null

  const barColor = (usedPct ?? 0) > 85 ? "#f87171" : (usedPct ?? 0) > 65 ? "#fbbf24" : "#a78bfa"

  const statusLabel = disk.is_osd
    ? <span className="text-xs text-[#4ade80]">In storage</span>
    : disk.is_builtin
      ? <span className="text-xs text-[#fbbf24]">Built-in · can&apos;t unplug</span>
      : <span className="text-xs text-[#fbbf24]">Safe to unplug</span>

  return (
    <Card>
      <CardContent className="pt-5">
        <div className="flex items-start gap-3">
          <div className="flex flex-col items-center gap-0.5 mt-0.5 flex-shrink-0">
            <button
              onClick={() => onMove("up")}
              disabled={isFirst}
              className="p-0.5 rounded hover:bg-[#27272a] disabled:opacity-20 disabled:cursor-not-allowed transition-colors"
            >
              <ChevronUp className="h-3.5 w-3.5 text-[#71717a]" strokeWidth={2} />
            </button>
            <button
              onClick={() => onMove("down")}
              disabled={isLast}
              className="p-0.5 rounded hover:bg-[#27272a] disabled:opacity-20 disabled:cursor-not-allowed transition-colors"
            >
              <ChevronDown className="h-3.5 w-3.5 text-[#71717a]" strokeWidth={2} />
            </button>
          </div>

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
                <span className="font-medium text-[#fafafa] text-sm">{disk.model || disk.name}</span>
                {disk.model && <span className="text-xs text-[#52525b] font-mono">{disk.name}</span>}
              </div>
              <div className="flex items-center gap-2">
                {statusLabel}
                <span className="text-xs text-[#52525b]">·</span>
                <span className="text-xs text-[#71717a]">{fmt(disk.size_bytes)}</span>
              </div>
            </div>

            <div className="text-xs text-[#52525b] mt-0.5">{disk.hostname}</div>

            {disk.is_osd && disk.used_bytes != null && disk.size_bytes > 0 && (
              <div className="mt-2 space-y-1">
                <div className="h-1.5 rounded-full bg-[#27272a] overflow-hidden">
                  <div
                    className="h-full rounded-full transition-all"
                    style={{ width: `${usedPct ?? 0}%`, background: barColor }}
                  />
                </div>
                <span className="text-xs text-[#71717a]">{fmt(disk.used_bytes)} used · {usedPct}%</span>
              </div>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

export function DisksPage() {
  const [disks, setDisks] = useState<DiskItem[]>([])
  const [ceph, setCeph] = useState<CephStatus | null>(null)
  const [saving, setSaving] = useState(false)

  function load() {
    fetch("/api/disks")
      .then(r => r.json())
      .then(d => setDisks(d as DiskItem[]))
      .catch(() => {})
    fetch("/api/ceph/status")
      .then(r => r.json())
      .then(d => setCeph(d as CephStatus))
      .catch(() => {})
  }

  useEffect(() => {
    load()
    const interval = setInterval(load, 10_000)
    return () => clearInterval(interval)
  }, [])

  async function moveItem(index: number, dir: "up" | "down") {
    const next = [...disks]
    const target = dir === "up" ? index - 1 : index + 1
    if (target < 0 || target >= next.length) return
    ;[next[index], next[target]] = [next[target], next[index]]
    setDisks(next)
    setSaving(true)
    try {
      const body: DiskOrderRequest = {
        entries: next.map(d => ({ host: d.host, disk_name: d.name })),
      }
      await fetch("/api/disks/order", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      })
    } finally {
      setSaving(false)
      load()
    }
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Disks at the top are preferred for storage. Move a disk down to free it up.
        </p>
      </div>

      <StorageOverview status={ceph} />

      {disks.length > 0 && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">Disks</h2>
            {saving && <span className="text-xs text-[#52525b]">Saving…</span>}
          </div>
          <div className="space-y-3">
            {disks.map((disk, i) => (
              <DiskRow
                key={`${disk.host}:${disk.name}`}
                disk={disk}
                isFirst={i === 0}
                isLast={i === disks.length - 1}
                onMove={dir => void moveItem(i, dir)}
              />
            ))}
          </div>
        </div>
      )}

      {disks.length === 0 && (
        <p className="text-sm text-[#52525b] text-center py-8">No storage disks detected</p>
      )}
    </div>
  )
}
