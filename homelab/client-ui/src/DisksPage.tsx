import { useEffect, useState } from "react";

interface Disk {
  name: string;
  model: string;
  size_bytes: number;
  host: string;
  storage_partition: string | null;
  storage_path: string | null;
}

function fmt(bytes: number): string {
  if (!bytes) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(1)} ${units[i]}`;
}

export function DisksPage() {
  const [disks, setDisks] = useState<Disk[] | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [done, setDone] = useState<Record<string, string>>({});

  useEffect(() => {
    fetch("/api/disks").then(r => r.json()).then(setDisks).catch(() => setDisks([]));
  }, []);

  async function exportDisk(disk: Disk) {
    const key = `${disk.host}:${disk.name}`;
    setBusy(key);
    setErrors(e => ({ ...e, [key]: "" }));
    const r = await fetch("/api/disks/enable-storage", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disk_name: disk.name, host: disk.host }),
    });
    const data = await r.json();
    setBusy(null);
    if (r.ok) {
      setDone(d => ({ ...d, [key]: data.path }));
    } else {
      setErrors(e => ({ ...e, [key]: data.detail ?? "Failed" }));
    }
  }

  if (!disks) return <div style={{ color: "#666" }}>Loading…</div>;
  if (disks.length === 0) return <div style={{ color: "#666" }}>No disks found.</div>;

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
          const exportedPath = done[key];
          const err = errors[key];

          return (
            <tr key={i} style={{ borderBottom: "1px solid #f3f4f6" }}>
              <td style={{ padding: "0.6rem 0.75rem" }}>
                <strong>{d.name}</strong>
                {d.model && <span style={{ color: "#888", marginLeft: "0.5rem" }}>{d.model}</span>}
              </td>
              <td style={{ padding: "0.6rem 0.75rem", color: "#666", fontSize: "0.8rem", fontFamily: "monospace" }}>
                {d.host}
              </td>
              <td style={{ padding: "0.6rem 0.75rem" }}>{fmt(d.size_bytes)}</td>
              <td style={{ padding: "0.6rem 0.75rem" }}>
                {exportedPath ? (
                  <span style={{ color: "#22c55e", fontFamily: "monospace", fontSize: "0.8rem" }}>
                    ✓ {exportedPath}
                  </span>
                ) : d.storage_path ? (
                  <div style={{ display: "flex", alignItems: "center", gap: "0.5rem", flexWrap: "wrap" }}>
                    <span style={{ color: "#aaa", fontFamily: "monospace", fontSize: "0.8rem" }}>{d.storage_path}</span>
                    <button
                      onClick={() => exportDisk(d)}
                      disabled={isBusy}
                      style={{
                        fontSize: "0.75rem", padding: "0.2rem 0.6rem", borderRadius: 4,
                        border: "1px solid #d1d5db", background: "#fff",
                        cursor: isBusy ? "not-allowed" : "pointer", color: "#1a1a1a",
                      }}
                    >
                      {isBusy ? "Exporting…" : "Export as NFS"}
                    </button>
                    {err && <span style={{ color: "#ef4444", fontSize: "0.75rem" }}>{err}</span>}
                  </div>
                ) : (
                  <span style={{ color: "#ccc", fontSize: "0.8rem" }}>No usable partition</span>
                )}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
