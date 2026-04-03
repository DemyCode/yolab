import { useEffect, useState } from "react";

interface Partition {
  name: string;
  size_bytes: number;
  mountpoint: string | null;
}

interface Disk {
  name: string;
  model: string;
  size_bytes: number;
  used_bytes: number;
  mountpoints: string[];
  partitions: Partition[];
  host: string;
  node_name?: string;
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

function MountModal({ partition, onClose, onMounted }: {
  partition: Partition;
  onClose: () => void;
  onMounted: () => void;
}) {
  const [path, setPath] = useState(`/mnt/${partition.name}`);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  async function mount() {
    setLoading(true);
    setError("");
    const r = await fetch("/api/disks/mount", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ device: `/dev/${partition.name}`, path }),
    });
    const data = await r.json();
    if (!r.ok) { setError(data.detail ?? "Failed"); setLoading(false); return; }
    onMounted();
  }

  return (
    <div style={{ position: "fixed", inset: 0, background: "rgba(0,0,0,0.4)", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 100 }}>
      <div style={{ background: "#fff", borderRadius: 12, padding: "2rem", width: "100%", maxWidth: 400 }}>
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: "1.25rem" }}>
          <h2 style={{ margin: 0, fontSize: "1.1rem" }}>Mount /dev/{partition.name}</h2>
          <button onClick={onClose} style={{ background: "none", border: "none", cursor: "pointer", fontSize: "1.2rem" }}>✕</button>
        </div>
        <label style={{ display: "block", fontSize: "0.78rem", fontWeight: "bold", marginBottom: 4 }}>Mount point</label>
        <input
          value={path}
          onChange={(e) => setPath(e.target.value)}
          style={{ width: "100%", border: "1px solid #e5e7eb", borderRadius: 6, padding: "0.4rem 0.6rem", fontSize: "0.9rem", boxSizing: "border-box", marginBottom: "1rem" }}
        />
        {error && <div style={{ color: "#ef4444", fontSize: "0.82rem", marginBottom: "0.75rem" }}>{error}</div>}
        <button
          onClick={mount}
          disabled={loading}
          style={{ width: "100%", padding: "0.6rem", background: loading ? "#999" : "#1a1a1a", color: "#fff", border: "none", borderRadius: 6, cursor: loading ? "not-allowed" : "pointer", fontWeight: "bold" }}
        >
          {loading ? "Mounting…" : "Mount"}
        </button>
      </div>
    </div>
  );
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [mounting, setMounting] = useState<Partition | null>(null);

  function load() {
    fetch("/api/disks").then((r) => r.json()).then(setDisks).catch((e) => setError(String(e)));
  }

  useEffect(() => { load(); }, []);

  if (error) return <div style={{ color: "red" }}>{error}</div>;
  if (!disks) return <div style={{ color: "#666" }}>Loading…</div>;
  if (disks.length === 0) return <div style={{ color: "#666" }}>No disks found.</div>;

  return (
    <>
      <table style={{ width: "100%", borderCollapse: "collapse", fontSize: "0.85rem" }}>
        <thead>
          <tr style={{ textAlign: "left", borderBottom: "2px solid #e5e7eb" }}>
            <th style={{ padding: "0.5rem 0.75rem" }}>Disk</th>
            <th style={{ padding: "0.5rem 0.75rem" }}>Node</th>
            <th style={{ padding: "0.5rem 0.75rem" }}>Size</th>
            <th style={{ padding: "0.5rem 0.75rem" }}>Used</th>
            <th style={{ padding: "0.5rem 0.75rem", minWidth: 160 }}>Usage</th>
          </tr>
        </thead>
        <tbody>
          {disks.map((d, i) => {
            const pct = d.size_bytes > 0 ? Math.round((d.used_bytes / d.size_bytes) * 100) : 0;
            return (
              <>
                <tr key={i} style={{ borderBottom: d.partitions.length ? "none" : "1px solid #f3f4f6" }}>
                  <td style={{ padding: "0.5rem 0.75rem" }}>
                    <strong>{d.name}</strong>
                    {d.model && <span style={{ color: "#888", marginLeft: "0.5rem", fontWeight: "normal" }}>{d.model}</span>}
                  </td>
                  <td style={{ padding: "0.5rem 0.75rem", fontSize: "0.8rem", color: "#666" }}>{d.node_name ?? d.host}</td>
                  <td style={{ padding: "0.5rem 0.75rem" }}>{fmt(d.size_bytes)}</td>
                  <td style={{ padding: "0.5rem 0.75rem" }}>{d.used_bytes > 0 ? fmt(d.used_bytes) : "—"}</td>
                  <td style={{ padding: "0.5rem 0.75rem" }}>
                    {d.used_bytes > 0 ? (
                      <div style={{ display: "flex", alignItems: "center", gap: "0.5rem" }}>
                        <FillBar used={d.used_bytes} total={d.size_bytes} />
                        <span style={{ whiteSpace: "nowrap", fontSize: "0.75rem", color: "#666", minWidth: 32 }}>{pct}%</span>
                      </div>
                    ) : "—"}
                  </td>
                </tr>
                {d.partitions.map((p, j) => (
                  <tr key={`${i}-${j}`} style={{ borderBottom: j === d.partitions.length - 1 ? "1px solid #f3f4f6" : "none", background: "#fafafa" }}>
                    <td style={{ padding: "0.3rem 0.75rem 0.3rem 2rem", fontSize: "0.8rem", color: "#555" }}>
                      ↳ {p.name} <span style={{ color: "#aaa" }}>{fmt(p.size_bytes)}</span>
                    </td>
                    <td />
                    <td />
                    <td style={{ padding: "0.3rem 0.75rem", fontSize: "0.78rem", color: "#888", fontFamily: "monospace" }}>
                      {p.mountpoint ?? <span style={{ color: "#bbb" }}>not mounted</span>}
                    </td>
                    <td style={{ padding: "0.3rem 0.75rem" }}>
                      {!p.mountpoint && (
                        <button
                          onClick={() => setMounting(p)}
                          style={{ fontSize: "0.75rem", padding: "0.2rem 0.6rem", borderRadius: 4, border: "1px solid #e5e7eb", background: "#fff", cursor: "pointer" }}
                        >
                          Mount
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </>
            );
          })}
        </tbody>
      </table>

      {mounting && (
        <MountModal
          partition={mounting}
          onClose={() => setMounting(null)}
          onMounted={() => { setMounting(null); load(); }}
        />
      )}
    </>
  );
}
