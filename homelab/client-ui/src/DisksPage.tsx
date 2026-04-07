import { useEffect, useState } from "react";

interface StoragePartition {
  name: string;
  mountpoint: string | null;
}

interface Disk {
  name: string;
  model: string;
  size_bytes: number;
  used_bytes: number;
  mountpoints: string[];
  host: string;
  node_name?: string;
  is_system: boolean;
  storage_partition: StoragePartition | null;
}

function fmt(bytes: number): string {
  if (bytes === 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(1)} ${units[i]}`;
}

function FillBar({ used, total }: { used: number; total: number }) {
  const pct = total > 0 ? Math.min(100, (used / total) * 100) : 0;
  const color = pct > 90 ? "#ef4444" : pct > 70 ? "#f59e0b" : "#22c55e";
  return (
    <div style={{ background: "#e5e7eb", borderRadius: 4, height: 10, flex: 1, overflow: "hidden" }}>
      <div style={{ width: `${pct}%`, background: color, height: "100%", borderRadius: 4 }} />
    </div>
  );
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [enabling, setEnabling] = useState<string | null>(null);
  const [enableError, setEnableError] = useState<Record<string, string>>({});

  function load() {
    fetch("/api/disks").then((r) => r.json()).then(setDisks).catch((e) => setError(String(e)));
  }

  useEffect(() => { load(); }, []);

  async function enableStorage(diskName: string) {
    setEnabling(diskName);
    setEnableError((prev) => ({ ...prev, [diskName]: "" }));
    const r = await fetch("/api/disks/enable-storage", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disk_name: diskName }),
    });
    const data = await r.json();
    setEnabling(null);
    if (!r.ok) {
      setEnableError((prev) => ({ ...prev, [diskName]: data.detail ?? "Failed" }));
    } else {
      load();
    }
  }

  if (error) return <div style={{ color: "red" }}>{error}</div>;
  if (!disks) return <div style={{ color: "#666" }}>Loading…</div>;
  if (disks.length === 0) return <div style={{ color: "#666" }}>No disks found.</div>;

  return (
    <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.85rem" }}>
      <thead>
        <tr style={{ textAlign: "left", borderBottom: "2px solid #e5e7eb" }}>
          <th style={{ padding: "0.5rem 0.75rem" }}>Disk</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Node</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Size</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Used</th>
          <th style={{ padding: "0.5rem 0.75rem", minWidth: 160 }}>Usage</th>
          <th style={{ padding: "0.5rem 0.75rem" }}>Status</th>
        </tr>
      </thead>
      <tbody>
        {disks.map((d, i) => {
          const pct = d.size_bytes > 0 ? Math.round((d.used_bytes / d.size_bytes) * 100) : 0;
          const err = enableError[d.name];

          let status: React.ReactNode;
          if (d.is_system) {
            status = <span style={{ color: "#888", fontSize: "0.8rem" }}>System disk</span>;
          } else if (d.storage_partition?.mountpoint) {
            status = (
              <span style={{ color: "#22c55e", fontSize: "0.8rem", fontFamily: "monospace" }}>
                {d.storage_partition.mountpoint}
              </span>
            );
          } else if (d.storage_partition) {
            status = (
              <div>
                <button
                  onClick={() => enableStorage(d.name)}
                  disabled={enabling === d.name}
                  style={{
                    fontSize: "0.78rem",
                    padding: "0.25rem 0.75rem",
                    borderRadius: 5,
                    border: "1px solid #d1d5db",
                    background: enabling === d.name ? "#f3f4f6" : "#fff",
                    cursor: enabling === d.name ? "not-allowed" : "pointer",
                    color: "#1a1a1a",
                  }}
                >
                  {enabling === d.name ? "Adding…" : "Add to storage"}
                </button>
                {err && <div style={{ color: "#ef4444", fontSize: "0.75rem", marginTop: 2 }}>{err}</div>}
              </div>
            );
          } else {
            status = <span style={{ color: "#bbb", fontSize: "0.8rem" }}>No usable partition</span>;
          }

          return (
            <tr key={i} style={{ borderBottom: "1px solid #f3f4f6" }}>
              <td style={{ padding: "0.6rem 0.75rem" }}>
                <strong>{d.name}</strong>
                {d.model && <span style={{ color: "#888", marginLeft: "0.5rem", fontWeight: "normal" }}>{d.model}</span>}
              </td>
              <td style={{ padding: "0.6rem 0.75rem", fontSize: "0.8rem", color: "#666" }}>{d.node_name ?? d.host}</td>
              <td style={{ padding: "0.6rem 0.75rem" }}>{fmt(d.size_bytes)}</td>
              <td style={{ padding: "0.6rem 0.75rem" }}>{d.used_bytes > 0 ? fmt(d.used_bytes) : "—"}</td>
              <td style={{ padding: "0.6rem 0.75rem" }}>
                {d.used_bytes > 0 ? (
                  <div style={{ display: "flex", alignItems: "center", gap: "0.5rem" }}>
                    <FillBar used={d.used_bytes} total={d.size_bytes} />
                    <span style={{ whiteSpace: "nowrap", fontSize: "0.75rem", color: "#666", minWidth: 32 }}>{pct}%</span>
                  </div>
                ) : "—"}
              </td>
              <td style={{ padding: "0.6rem 0.75rem" }}>{status}</td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
