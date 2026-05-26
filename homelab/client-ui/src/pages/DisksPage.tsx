import { useEffect, useState } from "react";
import { HardDrive, Share2, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";

interface AppUsage {
  name: string;
  bytes: number;
}

interface Disk {
  name: string;
  model: string;
  size_bytes: number;
  host: string;
  storage_partition: string | null;
  storage_path: string | null;
  fs_size_bytes: number;
  fs_used_bytes: number;
  app_usage: AppUsage[];
}

interface StorageEntry {
  host: string;
  path: string;
}

const APP_COLORS = [
  "#a78bfa",
  "#60a5fa",
  "#34d399",
  "#f87171",
  "#fbbf24",
  "#818cf8",
  "#f472b6",
  "#2dd4bf",
];

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

function UsageBar({ disk }: { disk: Disk }) {
  const total = disk.fs_size_bytes;
  if (!total) return null;

  const appTotal = disk.app_usage.reduce((s, a) => s + a.bytes, 0);
  const otherUsed = Math.max(0, disk.fs_used_bytes - appTotal);
  const free = Math.max(0, total - disk.fs_used_bytes);
  const pct = (bytes: number) => `${((bytes / total) * 100).toFixed(1)}%`;

  return (
    <div className="mt-3">
      <div className="flex h-2 rounded-full overflow-hidden bg-[#27272a] w-full">
        {disk.app_usage.map((app, i) => (
          <div
            key={app.name}
            title={`${app.name}: ${fmt(app.bytes)}`}
            style={{
              width: pct(app.bytes),
              background: APP_COLORS[i % APP_COLORS.length],
              flexShrink: 0,
            }}
          />
        ))}
        {otherUsed > 0 && (
          <div
            title={`Other: ${fmt(otherUsed)}`}
            style={{
              width: pct(otherUsed),
              background: "#52525b",
              flexShrink: 0,
            }}
          />
        )}
        {free > 0 && (
          <div
            title={`Free: ${fmt(free)}`}
            style={{ flex: 1, background: "#1f1f23" }}
          />
        )}
      </div>

      <div className="flex flex-wrap gap-x-4 gap-y-1 mt-2">
        {disk.app_usage.map((app, i) => (
          <span
            key={app.name}
            className="flex items-center gap-1.5 text-[11px] text-[#71717a]"
          >
            <span
              className="w-2 h-2 rounded-sm flex-shrink-0"
              style={{ background: APP_COLORS[i % APP_COLORS.length] }}
            />
            {app.name} · {fmt(app.bytes)}
          </span>
        ))}
        {otherUsed > 0 && (
          <span className="flex items-center gap-1.5 text-[11px] text-[#71717a]">
            <span className="w-2 h-2 rounded-sm bg-[#52525b] flex-shrink-0" />
            other · {fmt(otherUsed)}
          </span>
        )}
        <span className="flex items-center gap-1.5 text-[11px] text-[#52525b]">
          <span className="w-2 h-2 rounded-sm bg-[#27272a] flex-shrink-0" />
          free · {fmt(free)}
        </span>
      </div>
    </div>
  );
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[]>([]);
  const [exported, setExported] = useState<StorageEntry[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [errors, setErrors] = useState<Record<string, string>>({});

  function load() {
    fetch("/api/disks")
      .then((r) => r.json())
      .then((d) => setDisks(d as Disk[]))
      .catch(() => {});
    fetch("/api/storage")
      .then((r) => r.json())
      .then((d) => setExported(d as StorageEntry[]))
      .catch(() => {});
  }

  useEffect(() => {
    load();
  }, []);

  function isExported(disk: Disk) {
    return exported.some(
      (e) => e.host === disk.host && e.path === disk.storage_path,
    );
  }

  async function exportDisk(disk: Disk) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    setErrors((e) => ({ ...e, [key]: "" }));
    const r = await fetch("/api/disks/enable-storage", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
    });
    const data = (await r.json()) as { detail?: string };
    setBusy(null);
    if (r.ok) load();
    else setErrors((e) => ({ ...e, [key]: data.detail ?? "Failed" }));
  }

  async function unexportDisk(disk: Disk) {
    if (!disk.storage_path) return;
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    setErrors((e) => ({ ...e, [key]: "" }));
    const r = await fetch("/api/disks/disable-storage", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: disk.storage_path, host: disk.host }),
    });
    const data = (await r.json()) as { detail?: string };
    setBusy(null);
    if (r.ok) load();
    else setErrors((e) => ({ ...e, [key]: data.detail ?? "Failed" }));
  }

  return (
    <div className="space-y-6 max-w-3xl">
      <div>
        <h1 className="text-xl font-semibold text-[#fafafa]">Disks</h1>
        <p className="text-sm text-[#71717a] mt-0.5">
          Storage devices and NFS exports
        </p>
      </div>

      {disks.length === 0 ? (
        <p className="text-sm text-[#71717a]">No disks found.</p>
      ) : (
        <div className="space-y-3">
          {disks.map((d, i) => {
            const key = `${d.host}:${d.name}`;
            const isBusy = busy === key;
            const isExp = isExported(d);
            const err = errors[key];

            return (
              <Card key={i}>
                <CardContent className="pt-5">
                  <div className="flex items-start justify-between gap-4 flex-wrap">
                    {/* Left: disk info */}
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
                            {fmt(d.fs_size_bytes || d.size_bytes)}
                          </span>
                        </div>
                        {d.storage_path && (
                          <div className="flex items-center gap-1.5 mt-1">
                            <span
                              className={`text-[11px] font-mono ${isExp ? "text-[#4ade80]" : "text-[#52525b]"}`}
                            >
                              {d.storage_path}
                            </span>
                          </div>
                        )}
                      </div>
                    </div>

                    {/* Right: action */}
                    <div className="flex items-center gap-2 flex-shrink-0">
                      {err && (
                        <span className="text-xs text-[#f87171]">{err}</span>
                      )}
                      {d.storage_path ? (
                        isExp ? (
                          <Button
                            variant="destructive"
                            size="sm"
                            onClick={() => void unexportDisk(d)}
                            disabled={isBusy}
                          >
                            <XCircle className="h-3.5 w-3.5" strokeWidth={2} />
                            {isBusy ? "Removing…" : "Unexport"}
                          </Button>
                        ) : (
                          <Button
                            variant="outline"
                            size="sm"
                            onClick={() => void exportDisk(d)}
                            disabled={isBusy}
                          >
                            <Share2 className="h-3.5 w-3.5" strokeWidth={2} />
                            {isBusy ? "Exporting…" : "Export as NFS"}
                          </Button>
                        )
                      ) : (
                        <span className="text-xs text-[#3f3f46]">
                          No partition
                        </span>
                      )}
                    </div>
                  </div>

                  {isExp && d.fs_size_bytes > 0 && <UsageBar disk={d} />}
                </CardContent>
              </Card>
            );
          })}
        </div>
      )}
    </div>
  );
}
