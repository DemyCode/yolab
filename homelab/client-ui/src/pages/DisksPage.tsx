import { useEffect, useRef, useState } from "react";
import { GripVertical, HardDrive } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import type { CephStatus, DiskItem, DiskOrderRequest } from "@/types/disk";
import {
  DndContext,
  PointerSensor,
  KeyboardSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
  arrayMove,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";

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

function DiskRow({ disk }: { disk: DiskItem }) {
  const id = `${disk.host}:${disk.name}`;
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id });

  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
    opacity: isDragging ? 0.4 : 1,
    zIndex: isDragging ? 10 : undefined,
  };

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
    <div ref={setNodeRef} style={style}>
      <Card>
        <CardContent className="pt-5">
          <div className="flex items-start gap-3">
            <button
              {...attributes}
              {...listeners}
              className="mt-1 flex-shrink-0 p-0.5 rounded cursor-grab active:cursor-grabbing hover:bg-[#27272a] transition-colors touch-none"
              aria-label="Drag to reorder"
            >
              <GripVertical
                className="h-4 w-4 text-[#52525b]"
                strokeWidth={2}
              />
            </button>

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
                </div>
              </div>

              <div className="text-xs text-[#52525b] mt-0.5">
                {disk.hostname}
              </div>

              {disk.is_osd &&
                disk.used_bytes != null &&
                disk.size_bytes > 0 && (
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
                      {fmt(disk.used_bytes)} used · {usedPct}%
                    </span>
                  </div>
                )}
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

export function DisksPage() {
  const [disks, setDisks] = useState<DiskItem[]>([]);
  const [ceph, setCeph] = useState<CephStatus | null>(null);
  const [saving, setSaving] = useState(false);
  const savingRef = useRef(false);
  const draggingRef = useRef(false);

  function load() {
    // Skip refresh while user is dragging or saving — avoids flickering the list mid-interaction
    if (savingRef.current || draggingRef.current) return;
    fetch("/api/disks")
      .then((r) => r.json())
      .then((d: DiskItem[]) => {
        // Never replace a populated list with an empty one — the API can return [] briefly
        // during reconcile or when a pod is restarting, which would make the list disappear
        if (d.length > 0) setDisks(d);
      })
      .catch(() => {});
    fetch("/api/ceph/status")
      .then((r) => r.json())
      .then((d) => setCeph(d as CephStatus))
      .catch(() => {});
  }

  useEffect(() => {
    load();
    const interval = setInterval(load, 10_000);
    return () => clearInterval(interval);
  }, []);

  const sensors = useSensors(
    useSensor(PointerSensor),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );

  function handleDragStart() {
    draggingRef.current = true;
  }

  async function handleDragEnd(event: DragEndEvent) {
    draggingRef.current = false;
    const { active, over } = event;
    if (!over || active.id === over.id) return;

    const oldIndex = disks.findIndex(
      (d) => `${d.host}:${d.name}` === active.id,
    );
    const newIndex = disks.findIndex((d) => `${d.host}:${d.name}` === over.id);
    if (oldIndex === -1 || newIndex === -1) return;

    const next = arrayMove(disks, oldIndex, newIndex);
    setDisks(next);
    setSaving(true);
    savingRef.current = true;
    try {
      const body: DiskOrderRequest = {
        entries: next.map((d) => ({ host: d.host, disk_name: d.name })),
      };
      await fetch("/api/disks/order", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
    } finally {
      setSaving(false);
      savingRef.current = false;
    }
  }

  const diskIds = disks.map((d) => `${d.host}:${d.name}`);

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Storage</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Drag to reorder. Disks at the top are preferred for storage.
        </p>
      </div>

      <StorageOverview status={ceph} />

      {disks.length > 0 && (
        <div>
          <div className="flex items-center justify-between mb-3">
            <h2 className="text-xs font-semibold uppercase tracking-wider text-[#52525b]">
              Disks
            </h2>
            {saving && <span className="text-xs text-[#52525b]">Saving…</span>}
          </div>
          <DndContext
            sensors={sensors}
            collisionDetection={closestCenter}
            onDragStart={handleDragStart}
            onDragEnd={handleDragEnd}
          >
            <SortableContext
              items={diskIds}
              strategy={verticalListSortingStrategy}
            >
              <div className="space-y-3">
                {disks.map((disk) => (
                  <DiskRow key={`${disk.host}:${disk.name}`} disk={disk} />
                ))}
              </div>
            </SortableContext>
          </DndContext>
        </div>
      )}

      {disks.length === 0 && (
        <p className="text-sm text-[#52525b] text-center py-8">
          No storage disks detected
        </p>
      )}
    </div>
  );
}
