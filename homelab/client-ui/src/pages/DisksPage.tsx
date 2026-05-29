import { useEffect, useState } from "react";
import { HardDrive, Eraser } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";

interface Disk {
  name: string;
  model: string;
  size_bytes: number;
  host: string;
  status: "osd" | "pending_osd" | "needs_format" | "system";
  ceph_osd_id: number | null;
  fs_type: string | null;
}

interface CephStatus {
  available: boolean;
  health?: string;
  osd_count?: number;
  osd_up?: number;
  total_bytes?: number;
  used_bytes?: number;
  error?: string;
}

function fmt(bytes: number): string {
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

function healthColor(health: string) {
  if (health === "HEALTH_OK") return "#4ade80";
  if (health === "HEALTH_WARN") return "#fbbf24";
  return "#f87171";
}

function CephHealthBar({ status }: { status: CephStatus }) {
  if (!status.available) {
    return (
      <Card>
        <CardContent className="pt-4 pb-4">
          <div className="flex items-center gap-2">
            <span className="w-2 h-2 rounded-full bg-[#52525b] flex-shrink-0" />
            <span className="text-sm text-[#71717a]">
              Ceph cluster not ready
              {status.error ? ` — ${status.error}` : ""}
            </span>
          </div>
        </CardContent>
      </Card>
    );
  }

  const usedPct =
    status.total_bytes && status.used_bytes
      ? (status.used_bytes / status.total_bytes) * 100
      : 0;

  return (
    <Card>
      <CardContent className="pt-4 pb-4">
        <div className="flex items-center justify-between gap-4 flex-wrap">
          <div className="flex items-center gap-3">
            <span
              className="w-2 h-2 rounded-full flex-shrink-0"
              style={{ background: healthColor(status.health ?? "") }}
            />
            <span className="text-sm font-medium text-[#fafafa]">
              {status.health}
            </span>
            <span className="text-xs text-[#52525b]">·</span>
            <span className="text-xs text-[#71717a]">
              {status.osd_up}/{status.osd_count} OSDs up
            </span>
          </div>
          <div className="flex items-center gap-3">
            <div className="w-32 h-1.5 rounded-full bg-[#27272a] overflow-hidden">
              <div
                className="h-full rounded-full bg-[#a78bfa]"
                style={{ width: `${usedPct.toFixed(1)}%` }}
              />
            </div>
            <span className="text-xs text-[#71717a]">
              {fmt(status.used_bytes ?? 0)} / {fmt(status.total_bytes ?? 0)}
            </span>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

function statusBadge(disk: Disk) {
  switch (disk.status) {
    case "osd":
      return (
        <span className="text-[11px] font-mono text-[#4ade80]">
          OSD #{disk.ceph_osd_id}
        </span>
      );
    case "pending_osd":
      return (
        <span className="text-[11px] text-[#fbbf24]">Pending OSD…</span>
      );
    case "needs_format":
      return (
        <span className="text-[11px] text-[#f87171]">
          Needs format
          {disk.fs_type ? ` (${disk.fs_type})` : ""}
        </span>
      );
    case "system":
      return (
        <span className="text-[11px] text-[#52525b]">System disk</span>
      );
  }
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[]>([]);
  const [ceph, setCeph] = useState<CephStatus | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [errors, setErrors] = useState<Record<string, string>>({});

  function load() {
    fetch("/api/disks")
      .then((r) => r.json())
      .then((d) => setDisks(d as Disk[]))
      .catch(() => {});
    fetch("/api/ceph/status")
      .then((r) => r.json())
      .then((d) => setCeph(d as CephStatus))
      .catch(() => setCeph({ available: false }));
  }

  useEffect(() => {
    load();
  }, []);

  async function formatDisk(disk: Disk) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    setErrors((e) => ({ ...e, [key]: "" }));
    const r = await fetch("/api/disks/format", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
    });
    const data = (await r.json()) as { detail?: string };
    setBusy(null);
    if (r.ok) {
      // Rook picks the disk up within ~30s — reload after a short delay
      setTimeout(load, 5000);
    } else {
      setErrors((e) => ({ ...e, [key]: data.detail ?? "Failed" }));
    }
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Disks</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Storage devices managed by Ceph
        </p>
      </div>

      {ceph && <CephHealthBar status={ceph} />}

      {disks.length === 0 ? (
        <p className="text-sm text-[#71717a]">No disks found.</p>
      ) : (
        <div className="space-y-3">
          {disks.map((d, i) => {
            const key = `${d.host}:${d.name}`;
            const isBusy = busy === key;
            const err = errors[key];

            return (
              <Card key={i}>
                <CardContent className="pt-5">
                  <div className="flex items-start justify-between gap-4 flex-wrap">
                    <div className="flex items-start gap-3 min-w-0">
                      <div className="mt-0.5 rounded-md bg-[#27272a] p-1.5 flex-shrink-0">
                        <HardDrive
                          className="h-4 w-4 text-[#a1a1aa]"
                          strokeWidth={1.75}
                        />
                      </div>
                      <div className="min-w-0">
                        <div className="flex items-baseline gap-2">
                          <span className="font-medium text-[#fafafa] text-sm">
                            {d.name}
                          </span>
                          {d.model && (
                            <span className="text-xs text-[#52525b] truncate">
                              {d.model}
                            </span>
                          )}
                        </div>
                        <div className="flex items-center gap-3 mt-0.5">
                          <span className="font-mono text-xs text-[#71717a]">
                            {d.host}
                          </span>
                          <span className="text-xs text-[#71717a]">
                            {fmt(d.size_bytes)}
                          </span>
                        </div>
                        <div className="mt-1">{statusBadge(d)}</div>
                      </div>
                    </div>

                    <div className="flex items-center gap-2 flex-shrink-0">
                      {err && (
                        <span className="text-xs text-[#f87171]">{err}</span>
                      )}
                      {d.status === "needs_format" && (
                        <Button
                          variant="outline"
                          size="sm"
                          onClick={() => void formatDisk(d)}
                          disabled={isBusy}
                        >
                          <Eraser className="h-3.5 w-3.5" strokeWidth={2} />
                          {isBusy ? "Formatting…" : "Format & Add"}
                        </Button>
                      )}
                    </div>
                  </div>
                </CardContent>
              </Card>
            );
          })}
        </div>
      )}
    </div>
  );
}
