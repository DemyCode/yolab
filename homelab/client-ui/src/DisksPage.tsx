import { useEffect, useState } from "react";

interface AppUsage {
  name: string;
  bytes: number;
}

interface Disk {
  name: string;
  model: string;
  size_bytes: number;
  host: string;
  state: "system" | "ready" | "needs_format" | "error" | "unknown";
  error: string | null;
  storage_path: string | null;
  fs_size_bytes: number;
  fs_used_bytes: number;
  app_usage: AppUsage[];
}

const APP_COLORS = [
  "#6366f1",
  "#f59e0b",
  "#10b981",
  "#ef4444",
  "#8b5cf6",
  "#06b6d4",
  "#f97316",
  "#ec4899",
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

function StateBadge({ state }: { state: Disk["state"] }) {
  const map: Record<
    string,
    { label: string; color: string; bg: string }
  > = {
    system: { label: "System", color: "#3b82f6", bg: "#eff6ff" },
    ready: { label: "Ready", color: "#22c55e", bg: "#f0fdf4" },
    needs_format: { label: "Needs format", color: "#f59e0b", bg: "#fffbeb" },
    error: { label: "Error", color: "#ef4444", bg: "#fef2f2" },
    unknown: { label: "Detecting…", color: "#9ca3af", bg: "#f9fafb" },
  };
  const s = map[state] ?? map.unknown;
  return (
    <span
      style={{
        fontSize: "0.7rem",
        padding: "0.15rem 0.5rem",
        borderRadius: 99,
        background: s.bg,
        color: s.color,
        fontWeight: 600,
        whiteSpace: "nowrap",
      }}
    >
      {s.label}
    </span>
  );
}

function UsageBar({ disk }: { disk: Disk }) {
  const total = disk.fs_size_bytes;
  if (!total) return null;

  const appTotal = disk.app_usage.reduce((s, a) => s + a.bytes, 0);
  const otherUsed = Math.max(0, disk.fs_used_bytes - appTotal);
  const free = Math.max(0, total - disk.fs_used_bytes);
  const pct = (bytes: number) => `${((bytes / total) * 100).toFixed(1)}%`;

  return (
    <div style={{ marginTop: "0.6rem" }}>
      <div
        style={{
          display: "flex",
          height: 10,
          borderRadius: 5,
          overflow: "hidden",
          background: "#f3f4f6",
          width: "100%",
        }}
      >
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
            style={{ width: pct(otherUsed), background: "#9ca3af", flexShrink: 0 }}
          />
        )}
        {free > 0 && (
          <div title={`Free: ${fmt(free)}`} style={{ flex: 1, background: "#e5e7eb" }} />
        )}
      </div>
      <div
        style={{
          display: "flex",
          flexWrap: "wrap",
          gap: "0.4rem 0.75rem",
          marginTop: "0.35rem",
        }}
      >
        {disk.app_usage.map((app, i) => (
          <span
            key={app.name}
            style={{ display: "flex", alignItems: "center", gap: 4, fontSize: "0.72rem", color: "#555" }}
          >
            <span
              style={{
                width: 8,
                height: 8,
                borderRadius: 2,
                background: APP_COLORS[i % APP_COLORS.length],
                display: "inline-block",
                flexShrink: 0,
              }}
            />
            {app.name} {fmt(app.bytes)}
          </span>
        ))}
        {otherUsed > 0 && (
          <span style={{ display: "flex", alignItems: "center", gap: 4, fontSize: "0.72rem", color: "#555" }}>
            <span
              style={{
                width: 8, height: 8, borderRadius: 2, background: "#9ca3af",
                display: "inline-block", flexShrink: 0,
              }}
            />
            other {fmt(otherUsed)}
          </span>
        )}
        <span style={{ display: "flex", alignItems: "center", gap: 4, fontSize: "0.72rem", color: "#9ca3af" }}>
          <span
            style={{
              width: 8, height: 8, borderRadius: 2, background: "#e5e7eb",
              display: "inline-block", flexShrink: 0,
            }}
          />
          free {fmt(free)}
        </span>
      </div>
    </div>
  );
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [confirming, setConfirming] = useState<string | null>(null);

  function load() {
    fetch("/api/disks")
      .then((r) => r.json())
      .then(setDisks)
      .catch(() => {});
  }

  useEffect(() => {
    load();
    const id = setInterval(load, 10000);
    return () => clearInterval(id);
  }, []);

  async function formatDisk(disk: Disk) {
    const key = `${disk.host}:${disk.name}`;
    if (confirming !== key) {
      setConfirming(key);
      return;
    }
    setConfirming(null);
    setBusy(key);
    setErrors((e) => ({ ...e, [key]: "" }));
    const r = await fetch(`/api/disks/${disk.name}/format`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ host: disk.host }),
    });
    const data = await r.json();
    setBusy(null);
    if (r.ok) load();
    else setErrors((e) => ({ ...e, [key]: data.detail ?? "Format failed" }));
  }

  if (!disks.length)
    return <div style={{ color: "#666" }}>No disks found.</div>;

  return (
    <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.85rem" }}>
      <thead>
        <tr style={{ textAlign: "left", borderBottom: "2px solid #e5e7eb" }}>
          <th style={{ padding: "0.5rem 0.75rem" }}>Disk</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Node</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Size</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Storage</th>
        </tr>
      </thead>
      <tbody>
        {disks.map((d, i) => {
          const key = `${d.host}:${d.name}`;
          const isBusy = busy === key;
          const isConfirming = confirming === key;
          const err = errors[key];

          return (
            <tr key={i} style={{ borderBottom: "1px solid #f3f4f6" }}>
              <td style={{ padding: "0.6rem 0.75rem" }}>
                <strong>{d.name}</strong>
                {d.model && (
                  <span style={{ color: "#888", marginLeft: "0.5rem" }}>{d.model}</span>
                )}
              </td>
              <td
                style={{
                  padding: "0.6rem 0.75rem",
                  color: "#666",
                  fontSize: "0.8rem",
                  fontFamily: "monospace",
                }}
              >
                {d.host}
              </td>
              <td style={{ padding: "0.6rem 0.75rem", whiteSpace: "nowrap" }}>
                {fmt(d.fs_size_bytes || d.size_bytes)}
              </td>
              <td style={{ padding: "0.6rem 0.75rem" }}>
                <div style={{ display: "flex", alignItems: "center", gap: "0.5rem", flexWrap: "wrap" }}>
                  <StateBadge state={d.state} />

                  {d.storage_path && (
                    <span style={{ fontFamily: "monospace", fontSize: "0.8rem", color: "#666" }}>
                      {d.storage_path}
                    </span>
                  )}

                  {d.state === "needs_format" && (
                    <button
                      onClick={() => formatDisk(d)}
                      disabled={isBusy}
                      style={{
                        fontSize: "0.75rem",
                        padding: "0.2rem 0.6rem",
                        borderRadius: 4,
                        border: `1px solid ${isConfirming ? "#ef4444" : "#fca5a5"}`,
                        background: isConfirming ? "#ef4444" : "#fff",
                        color: isConfirming ? "#fff" : "#ef4444",
                        cursor: isBusy ? "not-allowed" : "pointer",
                      }}
                    >
                      {isBusy ? "Formatting…" : isConfirming ? "Confirm format" : "Format as ext4"}
                    </button>
                  )}

                  {(d.error || err) && (
                    <span style={{ color: "#ef4444", fontSize: "0.75rem" }}>
                      {d.error || err}
                    </span>
                  )}
                </div>

                {(d.state === "system" || d.state === "ready") && d.fs_size_bytes > 0 && (
                  <UsageBar disk={d} />
                )}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
