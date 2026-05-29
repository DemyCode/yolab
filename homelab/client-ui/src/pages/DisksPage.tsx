import { useEffect, useState } from "react";
import { HardDrive, Eraser, Server } from "lucide-react";
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

interface SystemOsd {
  exists: boolean;
  size_bytes: number | null;
  vg_free_bytes: number;
  ceph_osd_id: number | null;
}

function SystemOsdCard() {
  const [osd, setOsd] = useState<SystemOsd | null>(null);
  const [sizeInput, setSizeInput] = useState("");
  const [resizeInput, setResizeInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  function load() {
    fetch("/api/disks/system-osd")
      .then((r) => r.json())
      .then((d) => setOsd(d as SystemOsd))
      .catch(() => {});
  }

  useEffect(() => {
    load();
  }, []);

  async function create() {
    if (!sizeInput.trim()) return;
    setBusy(true);
    setError("");
    const r = await fetch("/api/disks/system-osd", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ size: sizeInput.trim() }),
    });
    setBusy(false);
    if (r.ok) {
      setSizeInput("");
      load();
    } else {
      const d = (await r.json()) as { detail?: string };
      setError(d.detail ?? "Failed");
    }
  }

  async function resize() {
    if (!resizeInput.trim()) return;
    setBusy(true);
    setError("");
    const r = await fetch("/api/disks/system-osd", {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ size: resizeInput.trim() }),
    });
    setBusy(false);
    if (r.ok) {
      setResizeInput("");
      load();
    } else {
      const d = (await r.json()) as { detail?: string };
      setError(d.detail ?? "Failed");
    }
  }

  async function remove() {
    if (!confirm("Remove the system-disk OSD? Data on this OSD will be lost if it is the only copy.")) return;
    setBusy(true);
    setError("");
    const r = await fetch("/api/disks/system-osd", { method: "DELETE" });
    setBusy(false);
    if (r.ok) {
      load();
    } else {
      const d = (await r.json()) as { detail?: string };
      setError(d.detail ?? "Failed");
    }
  }

  if (!osd) return null;

  return (
    <Card>
      <CardContent className="pt-5">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md bg-[#27272a] p-1.5 flex-shrink-0">
            <Server className="h-4 w-4 text-[#a1a1aa]" strokeWidth={1.75} />
          </div>
          <div className="flex-1 min-w-0">
            <div className="flex items-baseline gap-2">
              <span className="font-medium text-[#fafafa] text-sm">System disk</span>
              <span className="text-xs text-[#52525b]">/dev/pool/ceph</span>
            </div>
            <p className="text-xs text-[#71717a] mt-0.5">
              Allocate space on the system disk as a Ceph OSD — no repartitioning needed.
            </p>

            <div className="mt-3 space-y-3">
              {osd.exists ? (
                <>
                  <div className="flex items-center gap-3 text-xs">
                    <span className="text-[#4ade80]">
                      {osd.ceph_osd_id !== null ? `OSD #${osd.ceph_osd_id} active` : "LV created, OSD pending…"}
                    </span>
                    <span className="text-[#52525b]">·</span>
                    <span className="text-[#71717a]">{fmt(osd.size_bytes ?? 0)} allocated</span>
                    <span className="text-[#52525b]">·</span>
                    <span className="text-[#71717a]">{fmt(osd.vg_free_bytes)} free on disk</span>
                  </div>
                  <div className="flex items-center gap-2">
                    <input
                      className="w-28 bg-[#18181b] border border-[#3f3f46] rounded px-2 py-1 text-xs text-[#fafafa] outline-none focus:border-[#a78bfa]"
                      placeholder="e.g. 500G"
                      value={resizeInput}
                      onChange={(e) => setResizeInput(e.target.value)}
                    />
                    <Button variant="outline" size="sm" onClick={() => void resize()} disabled={busy || !resizeInput.trim()}>
                      {busy ? "Extending…" : "Extend to"}
                    </Button>
                    <Button variant="outline" size="sm" onClick={() => void remove()} disabled={busy}
                      className="text-[#f87171] border-[#7f1d1d] hover:border-[#f87171]">
                      Remove
                    </Button>
                  </div>
                </>
              ) : (
                <>
                  <div className="text-xs text-[#71717a]">
                    No OSD allocated · {fmt(osd.vg_free_bytes)} free on system disk
                  </div>
                  <div className="flex items-center gap-2">
                    <input
                      className="w-28 bg-[#18181b] border border-[#3f3f46] rounded px-2 py-1 text-xs text-[#fafafa] outline-none focus:border-[#a78bfa]"
                      placeholder="e.g. 200G"
                      value={sizeInput}
                      onChange={(e) => setSizeInput(e.target.value)}
                    />
                    <Button variant="outline" size="sm" onClick={() => void create()} disabled={busy || !sizeInput.trim()}>
                      {busy ? "Creating…" : "Create OSD"}
                    </Button>
                  </div>
                </>
              )}
              {error && <p className="text-xs text-[#f87171]">{error}</p>}
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
  );
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

      <SystemOsdCard />

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
